//! #2755 R3 provisioning helpers extracted from `checkout.rs` to keep that handler
//! under the MCP-handler LOC ceiling (the same split pattern as `source_resolve.rs`):
//! the post-rollback response mapping and the marker content-durability fsync.

use super::checkout_txn::RollbackOutcome;
use serde_json::{json, Value};
use std::path::Path;

/// #2755 structured redaction: replace absolute filesystem paths (and Windows
/// drive paths) in an error string returned over the wire with `<path>`.
pub(super) fn redact_paths(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?P<b>^|[^\w])(?P<p>[A-Za-z]:\\[\w.\\@~%+-]+|(?:/[\w.@~%+-]+){2,})")
            .expect("valid redaction regex")
    });
    re.replace_all(s, "${b}<path>").into_owned()
}

/// #1447: resolve the checkout source repo from `repository_path` — the
/// cross-tool standard name used by bind_self / team update. Returns `None`
/// when absent or empty.
pub(crate) fn checkout_source(args: &Value) -> Option<&str> {
    args.get("repository_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Echo the actual provisioned HEAD only when the caller supplied an exact-head
/// expectation; the worktree remains the source of truth for the observed value.
pub(super) fn annotate_actual_head(resp: &mut Value, expected: Option<&str>, worktree: &Path) {
    if let Some(expected) = expected {
        let actual =
            crate::git_helpers::git_cmd(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
        resp["actual_head"] = json!(actual.trim());
        resp["expected_head"] = json!(expected);
    }
}

/// Remove only a branch authored by this checkout transaction after worktree-add
/// failure; pre-existing refs are never touched.
pub(super) fn rollback_auto_created_branch_if_needed(
    source: &Path,
    branch: &str,
    expected_head: &str,
    should_rollback: bool,
) {
    if should_rollback {
        super::checkout_disposable::rollback_auto_created_branch(source, branch, expected_head);
    }
}

#[cfg(all(test, unix))]
use std::cell::RefCell;

/// #1466: record every `repo action=checkout` outcome — success AND every
/// error path — to the daemon-observable event-log, so a silently-failed
/// checkout (e.g. the partial-worktree bootstrap race that motivated #1466:
/// `src/` present but no `.git`) leaves a diagnosable trace. Reuses
/// `event_log::log` (the same freeform-msg helper as `worktree_released_full`
/// — no new schema). Best-effort: `event_log::log` is fire-and-forget, so a
/// logging failure can never affect the checkout result (observability must
/// not become an availability risk). Logging once at the single wrapper exit
/// guarantees coverage of all current and future return paths.
pub(super) fn log_checkout_outcome(home: &Path, args: &Value, instance_name: &str, result: &Value) {
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    let source = checkout_source(args).unwrap_or("");
    let ok = result.get("error").is_none();
    let mut msg = format!("branch={branch} source={source} ok={ok}");
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        msg.push_str(&format!(" err={err}"));
    }
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        msg.push_str(&format!(" path={path}"));
    }
    crate::event_log::log(home, "worktree_checkout", instance_name, &msg);
}

/// #2755 R3 (root + independent review): map a post-`git worktree add`
/// [`RollbackOutcome`] to the checkout error response, reporting the ACTUAL cleanup
/// state. `Removed` → the historical "worktree rolled back" text. `RollbackPending`
/// → a STRUCTURED pending state (`code: "rollback_pending"`, `rollback_pending: true`)
/// that NEVER claims the worktree was rolled back — the remove failed (Windows
/// open-handle / transient FS) and the worktree survives for the recovery sweep.
/// `intent_durable=false` (the retained-intent journal save ALSO failed) is surfaced
/// for intervention. The original failure `code`/`stage` are preserved
/// (`failed_code`/`stage`) so machine consumers keep the root cause. Pure —
/// unit-tested cross-platform.
pub(super) fn rollback_response(
    outcome: RollbackOutcome,
    reason: &str,
    code: &str,
    stage: &str,
    branch: &str,
) -> Value {
    match outcome {
        RollbackOutcome::Removed => json!({
            "error": format!("{reason}, worktree rolled back"),
            "code": code,
            "stage": stage,
            "branch": branch,
        }),
        RollbackOutcome::RollbackPending { intent_durable } => json!({
            "error": format!(
                "{reason}; worktree REMOVE FAILED — rollback pending, recovery sweep will retry{}",
                if intent_durable {
                    ""
                } else {
                    " (retained-intent journal save ALSO failed — operator intervention needed)"
                }
            ),
            "code": "rollback_pending",
            "rollback_pending": true,
            "intent_durable": intent_durable,
            "failed_code": code,
            "stage": stage,
            "branch": branch,
        }),
    }
}

/// #2755 R3 (independent P1.4): fsync the `.agend-managed` marker file's CONTENTS
/// durable — `std::fs::write` + a parent-dir fsync makes the DIRENT durable but not
/// the bytes, so a crash/power loss could leave a durable journal phase (or Committed
/// success) with an empty/torn marker. Open + `sync_all()` and OBSERVE the result; a
/// failure aborts the transaction fail-closed. A `cfg(test)` thread-local seam forces
/// the sync error so the crash/durability rollback path is testable cross-platform.
///
/// The handle is opened for WRITE (not read-only): on Windows `sync_all` maps to
/// `FlushFileBuffers`, which requires `GENERIC_WRITE` and returns ACCESS_DENIED on a
/// read-only handle (`File::open`). `write(true)` (no truncate) preserves the bytes and
/// yields a flushable handle on every platform.
pub(super) fn sync_marker_contents(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_MARKER_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other(
            "test seam: forced marker sync_all failure",
        ));
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

#[cfg(test)]
thread_local! {
    static FAIL_MARKER_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: arm/disarm the [`sync_marker_contents`] failure seam (current thread).
#[cfg(test)]
pub(super) fn set_fail_marker_sync(fail: bool) {
    FAIL_MARKER_SYNC.with(|c| c.set(fail));
}

#[cfg(all(test, unix))]
thread_local! {
    static AFTER_EXPECTED_HEAD_VALIDATION: RefCell<Option<Box<dyn Fn()>>> =
        const { RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(super) struct ExpectedHeadValidationHookGuard;

#[cfg(all(test, unix))]
impl Drop for ExpectedHeadValidationHookGuard {
    fn drop(&mut self) {
        AFTER_EXPECTED_HEAD_VALIDATION.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Test-only seam used to deterministically move a ref after the precondition
/// check and before provisioning begins.
#[cfg(all(test, unix))]
pub(super) fn install_expected_head_validation_hook(
    hook: impl Fn() + 'static,
) -> ExpectedHeadValidationHookGuard {
    AFTER_EXPECTED_HEAD_VALIDATION.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    ExpectedHeadValidationHookGuard
}

#[cfg(all(test, unix))]
pub(super) fn hit_expected_head_validation_hook() {
    AFTER_EXPECTED_HEAD_VALIDATION.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook();
        }
    });
}

/// #6: optional exact-head precondition — BEFORE any branch creation so a
/// mismatch returns a structured error with zero mutation.
pub(super) fn validate_expected_head(
    args: &Value,
    source_path: &str,
    branch: &str,
) -> Option<Value> {
    let expected = args["expected_head"].as_str()?;
    let is_full_hex = (expected.len() == 40 || expected.len() == 64)
        && expected.chars().all(|c| c.is_ascii_hexdigit());
    if !is_full_hex {
        return Some(json!({
            "error": format!("expected_head must be a full 40/64-hex SHA, got '{expected}'"),
            "code": "invalid_expected_head",
        }));
    }
    let src = Path::new(source_path);
    let verify = crate::git_helpers::git_cmd(
        src,
        &["rev-parse", "--verify", &format!("{expected}^{{commit}}")],
    );
    if verify.is_err() {
        return Some(json!({
            "error": format!(
                "expected_head {expected} does not exist as a commit in the repository"
            ),
            "code": "expected_head_mismatch",
            "expected_head": expected,
            "actual_head": "",
        }));
    }
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = crate::git_helpers::git_cmd(src, &["rev-parse", "--verify", &branch_ref]);
    let actual = if let Ok(sha) = branch_exists {
        sha.trim().to_string()
    } else {
        let default_base = format!("origin/{}", crate::git_helpers::default_branch(src));
        let from_ref = args["from_ref"].as_str().unwrap_or(&default_base);
        crate::git_helpers::git_cmd(src, &["rev-parse", from_ref])
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if !actual.eq_ignore_ascii_case(expected) {
        return Some(json!({
            "error": format!("expected_head {expected} does not match branch HEAD {actual}"),
            "code": "expected_head_mismatch",
            "expected_head": expected,
            "actual_head": actual,
        }));
    }
    #[cfg(all(test, unix))]
    hit_expected_head_validation_hook();
    None
}

/// Return the exact expected commit as the creation base when the requested
/// branch is absent. The caller has already validated that the supplied
/// from_ref resolves to this commit; pinning the git operation prevents a
/// movable from_ref from changing between validation and branch creation.
pub(super) fn expected_creation_ref(
    args: &Value,
    source_path: &str,
    branch: &str,
) -> Option<String> {
    let expected = args["expected_head"].as_str()?;
    let branch_ref = format!("refs/heads/{branch}");
    let exists = crate::git_helpers::git_cmd(
        Path::new(source_path),
        &["rev-parse", "--verify", &branch_ref],
    )
    .is_ok();
    (!exists).then(|| expected.to_string())
}

/// Compare the provisioned worktree (and optionally its source branch) with
/// the exact expected commit. Returns the observed mismatching HEAD.
pub(super) fn expected_head_drift_actual(
    worktree: &Path,
    source: &Path,
    branch: &str,
    expected: &str,
    check_branch: bool,
) -> Option<String> {
    let actual = crate::git_helpers::git_cmd(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
    let actual = actual.trim().to_string();
    if !actual.eq_ignore_ascii_case(expected) {
        return Some(actual);
    }
    if check_branch {
        let branch_ref = format!("refs/heads/{branch}");
        let branch_actual =
            crate::git_helpers::git_cmd(source, &["rev-parse", "--verify", &branch_ref])
                .unwrap_or_default();
        let branch_actual = branch_actual.trim().to_string();
        if !branch_actual.eq_ignore_ascii_case(expected) {
            return Some(branch_actual);
        }
    }
    None
}

/// Roll back a fresh checkout whose final HEAD no longer satisfies
/// expected_head, preserving the structured cleanup outcome.
#[allow(clippy::too_many_arguments)]
pub(super) fn rollback_if_expected_head_drift(
    home: &Path,
    mangled: &str,
    journal: &mut super::checkout_txn::Journal,
    now: chrono::DateTime<chrono::Utc>,
    remove_worktree: impl Fn() -> bool,
    source: &Path,
    branch: &str,
    expected: &str,
    worktree: &Path,
    check_branch: bool,
    auto_created_branch: bool,
    stage: &str,
) -> Option<Value> {
    let actual = expected_head_drift_actual(worktree, source, branch, expected, check_branch)?;
    let outcome =
        super::checkout_txn::rollback_failed(home, mangled, journal, now, remove_worktree, || {});
    if matches!(outcome, RollbackOutcome::Removed) && auto_created_branch {
        let branch_ref = format!("refs/heads/{branch}");
        let _ =
            crate::git_helpers::git_bypass(source, &["update-ref", "-d", &branch_ref, expected]);
    }
    let mut err = rollback_response(
        outcome,
        "expected_head changed during checkout",
        "expected_head_drift",
        stage,
        branch,
    );
    err["expected_head"] = json!(expected);
    err["actual_head"] = json!(actual);
    Some(err)
}
