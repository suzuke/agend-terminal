//! #2158 GR1: the dispatch-time ci-watch auto-arm, split out of `dispatch_hook/mod.rs`.
//!
//! Two reasons for the extraction: (1) keep `mod.rs` under its file-size ceiling, and
//! (2) make arming a clean unit reached ONLY when the caller passed an explicit
//! `arm_ci_watch=true` (a real dispatch). Out-of-dispatch self-claims — `bind_self`
//! provisioning — pass `arm_ci_watch=false` and never call this, so they no longer
//! silently arm a watch the operator never asked for (the #2158 GR1 fix). The intent
//! is the caller's explicit bool, NOT `task_id` presence: a single-target
//! `send kind=task` (auto-create-exempt) legitimately reaches dispatch with an empty
//! task_id and MUST still arm.

use serde_json::json;
use std::path::Path;

/// Arm the dispatch ci-watch for `target` on `repo`+`branch`. Best-effort: a failed
/// arm is logged (never fatal — the dispatch + lease already succeeded).
pub(super) fn arm(
    home: &Path,
    target: &str,
    repo: &str,
    branch: &str,
    next_after_ci: Option<&str>,
    review_class: Option<&str>,
    task_id: &str,
) {
    // #931 Fix 2 (H5a): propagate the dispatcher's `next_after_ci` chain target into
    // the watch so the poll loop fires `[ci-ready-for-action]` to it on CI pass.
    // Explicit-only since t-ci-ready-pr2 dropped the #1037 `<team>-reviewer`
    // name-derive; unset → no chain target (subscribers still get the informational
    // `[ci-pass]`, #1796).
    let effective_next = next_after_ci.filter(|s| !s.is_empty()).map(String::from);
    let mut watch_args = json!({"repository": repo, "branch": branch});
    if let Some(ref next) = effective_next {
        watch_args["next_after_ci"] = json!(next);
    }
    // #1031: persist the dispatch task_id so the ci_check_repo emit can back-link the
    // `[ci-ready-for-action]` event to the originating dispatch (verdict correlation).
    if !task_id.is_empty() {
        watch_args["task_id"] = json!(task_id);
    }
    // #1877: a `second_reviewer=true` dispatch arms `review_class=dual` (poller enforces).
    if let Some(rc) = review_class.filter(|s| !s.is_empty()) {
        watch_args["review_class"] = json!(rc);
    }
    let watch_result = crate::mcp::handlers::ci::handle_watch_ci(home, &watch_args, target);
    // #1750 A1: surface a failed arm as an error log, not a false success.
    if let Some((code, err)) = super::auto_watch_arm_error(&watch_result) {
        tracing::error!(
            %target, repo, %branch, code, error = %err,
            "dispatch auto-watch_ci FAILED — no CI watch armed (ci-ready will not fire)"
        );
    } else {
        tracing::info!(
            %target, repo, %branch,
            next_after_ci = ?effective_next,
            explicit = next_after_ci.is_some(),
            "dispatch auto-watch_ci"
        );
    }
}
