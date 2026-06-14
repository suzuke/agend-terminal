//! Verification/reproduction tests for the `mcp-dispatch-comms` review batch.
//!
//! Attached as an in-module submodule of `dispatch_hook` so the
//! `pub(crate)` `ensure_branch_exists` entry point (and the `ErrorCode` /
//! `Stage` enums it returns) are reachable via `crate::mcp::handlers::dispatch_hook::`.
//!
//! Mirrors the fixture/harness conventions of the sibling
//! `dispatch_hook::tests` module (see `setup_test_repo` /
//! `ensure_branch_exists_*` there) — a temp `home`, a real on-disk git repo
//! created under `workspace/<agent>`, and a direct call to the production
//! `ensure_branch_exists` function (§1.4 "call the production fn directly").

#![allow(clippy::expect_used)]

use std::path::PathBuf;

/// Build an isolated temp HOME plus a real git repo at
/// `workspace/<agent>` with an `origin` pointing at a dead `file://` URL
/// and `refs/remotes/origin/main` populated — so `validate_branch(from_ref)`
/// passes for `origin/main` and no real network I/O is needed. Mirrors
/// `dispatch_hook::tests::setup_test_repo`.
#[cfg(unix)] // sole caller (F1) is #[cfg(unix)]; avoids a dead_code warning on Windows
fn mk_home_repo(tag: &str) -> (PathBuf, PathBuf) {
    let home = std::env::temp_dir().join(format!(
        "agend-mcp-dispatch-comms-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let repo = crate::paths::workspace_dir(&home).join("agent");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    let git = |args: &[&str]| -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git spawn")
    };
    let _ = git(&["init", "-b", "main"]);
    let _ = git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let _ = git(&[
        "remote",
        "add",
        "origin",
        "file:///dev/null/agend-fixture-mcp-dispatch-comms",
    ]);
    let main_sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .expect("utf8 sha")
        .trim()
        .to_string();
    if !main_sha.is_empty() {
        let _ = git(&["update-ref", "refs/remotes/origin/main", &main_sha]);
    }
    (home, repo)
}

/// Finding 1 (medium / security): agent-supplied `branch` reaches
/// `git branch <branch> <from_ref>` WITHOUT `validate_branch`. The
/// `from_ref` arg is validated (line ~730) but `branch` is not, so an
/// option-injection branch like `--upload-pack=/bin/sh` is handed to
/// `git branch` as the first positional — i.e. as an OPTION — before any
/// guard fires.
///
/// CORRECT behaviour (what this test pins): a charset-illegal /
/// option-injection `branch` must be rejected at the *validation* boundary
/// of `ensure_branch_exists`, BEFORE the `git branch` subprocess runs.
///
/// RED now: with the missing guard, the malicious `branch` flows to
/// `git branch --upload-pack=/bin/sh origin/main`, git rejects it with
/// "unknown option", and `ensure_branch_exists` reports
/// `code == ErrorCode::BranchCreateFailed` / `stage == Stage::CreateBranch`
/// — i.e. the failure was raised by git's own option parser, AFTER the
/// injected option already reached the subprocess. The asserts below fail.
///
/// GREEN after fix: adding `validate_branch(branch)` (mirroring the
/// `from_ref` guard) rejects the value at the daemon API boundary, so the
/// error is raised at a VALIDATION stage (not a git-create stage) and
/// `git branch` is never invoked with the injected option.
#[test]
#[cfg(unix)]
#[ignore = "mcp-dispatch-comms F1: red until validate_branch(branch) added to ensure_branch_exists; remove #[ignore] after fix"]
fn ensure_branch_exists_rejects_option_injection_branch_before_git_mcp_dispatch_comms() {
    let (home, repo) = mk_home_repo("f1");

    // `from_ref` is the production-default `origin/main` (passes the
    // existing `validate_branch(from_ref)` gate). The MALICIOUS value is
    // the user-controlled `branch` arg.
    let malicious_branch = "--upload-pack=/bin/sh";
    let result = crate::mcp::handlers::dispatch_hook::ensure_branch_exists(
        &home,
        &repo,
        malicious_branch,
        "origin/main",
        "agent",
    );

    let err = result.expect_err(
        "an option-injection branch name must be REJECTED by ensure_branch_exists, \
         not passed through to `git branch <branch>`",
    );

    // The rejection must come from the validation boundary, NOT from git's
    // own option parser after the injected option already reached the
    // subprocess. Pre-fix the error is BranchCreateFailed/CreateBranch.
    assert_ne!(
        err.code,
        crate::mcp::handlers::dispatch_hook::ErrorCode::BranchCreateFailed,
        "branch '{malicious_branch}' reached `git branch` as an option and git rejected it \
         (code={:?}, stage={:?}); validate_branch(branch) must reject it BEFORE the subprocess",
        err.code,
        err.stage,
    );
    assert_ne!(
        err.stage,
        crate::mcp::handlers::dispatch_hook::Stage::CreateBranch,
        "rejection must be raised at a validation stage, not the git-create stage \
         (got stage={:?})",
        err.stage,
    );
    assert_ne!(
        err.stage,
        crate::mcp::handlers::dispatch_hook::Stage::RetryCreate,
        "rejection must be raised at a validation stage, not the post-fetch retry-create stage \
         (got stage={:?})",
        err.stage,
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Helper shared by the two source-scanning invariants below: read the
/// body of `fn ensure_branch_exists` from this module's defining file.
fn ensure_branch_exists_body() -> String {
    let mod_rs =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/dispatch_hook/mod.rs");
    let text = std::fs::read_to_string(&mod_rs).expect("read dispatch_hook/mod.rs");
    extract_fn_body(&text, "fn ensure_branch_exists")
}

/// Extract the `{ ... }` body of the first function whose signature line
/// contains `sig`. Brace-balanced, comment/string naive but adequate for
/// these guards (the targeted needles never appear in string literals in
/// this function).
fn extract_fn_body(text: &str, sig: &str) -> String {
    let start = text
        .find(sig)
        .unwrap_or_else(|| panic!("fn `{sig}` not found"));
    let brace = text[start..]
        .find('{')
        .map(|o| start + o)
        .unwrap_or_else(|| panic!("opening brace for `{sig}` not found"));
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut end = brace;
    for (i, &b) in bytes.iter().enumerate().skip(brace) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    text[brace..=end].to_string()
}

fn strip_comments_and_blank(body: &str) -> String {
    body.lines()
        .map(|l| l.trim_start())
        .filter(|t| !t.starts_with("//") && !t.starts_with('*') && !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Finding 2 (low / maintainability): the `ensure_branch_exists` doc
/// comment claims the option-injection rule is "the same rule applied to
/// the user-supplied `branch` arg" — but the implementation only validates
/// `from_ref`. The comment is only HONEST once `validate_branch(branch)`
/// actually runs in the function body.
///
/// RED now: the function body contains `validate_branch(from_ref)` but no
/// `validate_branch(branch)` call, so the doc comment's claim is false.
///
/// GREEN after fix: the preferred resolution (add the missing
/// `validate_branch(branch)` call) makes the comment's claim true; this
/// scan then finds the call.
#[test]
#[ignore = "mcp-dispatch-comms F2: red until validate_branch(branch) present (doc comment claims it is); remove #[ignore] after fix"]
fn ensure_branch_exists_doc_claim_matches_code_mcp_dispatch_comms() {
    let body = strip_comments_and_blank(&ensure_branch_exists_body());
    // The doc comment promises `branch` is validated by the same rule as
    // `from_ref`. That promise is honoured only when this call exists.
    let validates_branch = body.contains("validate_branch(branch)")
        || body.contains("validate_branch(&branch)")
        || body.contains("validate_branch(branch,");
    assert!(
        validates_branch,
        "ensure_branch_exists doc comment claims the user-supplied `branch` arg runs through \
         `validate_branch`, but no `validate_branch(branch)` call exists in the function body. \
         Either add the call (preferred) or correct the comment."
    );
}

/// Finding 3 (medium / correctness): dispatch-time `git fetch` inside
/// `ensure_branch_exists` is bounded by `NETWORK_GIT_TIMEOUT` (300s).
/// `ensure_branch_exists` is on the synchronous pre-send path of
/// `handle_delegate_task`, and the MCP proxy classifies `send` with the
/// 30s `DEFAULT_TOOL_TIMEOUT`. So a dispatch with a `branch` can keep the
/// fetch running for up to 300s while the proxy already returned
/// `accepted_in_progress` (false success) at 30s, and a later bind failure
/// is then swallowed by the orphaned background thread.
///
/// Interim static guard (the runtime false-success arises from the
/// proxy's timeout-then-background-completion design — see redesign_note):
/// the dispatch-time fetches in `ensure_branch_exists` must NOT use the
/// unbounded 300s `NETWORK_GIT_TIMEOUT`. The fix bounds them under the
/// `send` proxy budget (a dispatch-fetch budget) or otherwise removes the
/// 300s ceiling from this on-the-send-path function.
///
/// RED now: the body bounds its fetches with `NETWORK_GIT_TIMEOUT`.
/// GREEN after fix: those fetches use a bounded dispatch budget instead.
#[test]
#[ignore = "mcp-dispatch-comms F3: red until dispatch-time fetch budget bounded under the send proxy timeout; remove #[ignore] after fix"]
fn ensure_branch_exists_fetch_budget_under_send_timeout_mcp_dispatch_comms() {
    let body = strip_comments_and_blank(&ensure_branch_exists_body());
    assert!(
        !body.contains("NETWORK_GIT_TIMEOUT"),
        "ensure_branch_exists runs on the synchronous pre-send path of handle_delegate_task, \
         but bounds its `git fetch` calls with the 300s NETWORK_GIT_TIMEOUT — far beyond the 30s \
         DEFAULT_TOOL_TIMEOUT the MCP proxy applies to `send`. A dispatch can therefore run up to \
         300s while the proxy already reported `accepted_in_progress`, and a later bind failure is \
         swallowed. Bound the dispatch-time fetch under the send budget."
    );
}
