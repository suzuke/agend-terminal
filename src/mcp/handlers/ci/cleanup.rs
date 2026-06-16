use serde_json::{json, Value};
use std::path::Path;

/// #789 — explicit MCP entry point for cleaning empty `init` commits
/// from a bound worktree. Operators / agents call this before push to
/// scrub backend session-checkpoint pollution that accumulated between
/// `dispatch_auto_bind_lease` (bind-time cleanup) and now.
///
/// Args:
/// - `agent` (optional, defaults to caller's `instance_name`): which
///   agent's bound worktree to clean.
///
/// Response shape (idempotent, observable):
/// - `{cleaned_count: N}` — N empty inits removed (0 = noop)
/// - `{cleaned_count: 0, skipped_reason: "no binding"}` — agent has no
///   active binding; explicit reason so callers don't mistake for noop
/// - `{error: "...", code: "cleanup_failed"}` — git subprocess failure
pub(crate) fn handle_cleanup_init_commits(home: &Path, args: &Value, instance_name: &str) -> Value {
    let agent = args["instance"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    if agent.is_empty() {
        return json!({
            "error": "missing 'instance' (and no caller instance_name available)",
            "code": "needs_agent"
        });
    }
    let worktree = match crate::binding::read(home, agent)
        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
    {
        Some(wt) => wt,
        None => {
            return json!({
                "cleaned_count": 0,
                "skipped_reason": format!("no binding for agent '{agent}'"),
            });
        }
    };
    match crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&worktree) {
        Ok(count) => json!({"cleaned_count": count}),
        Err(msg) => json!({
            "error": msg,
            "code": "cleanup_failed",
        }),
    }
}

/// #817: handle `repo action=cleanup_merged_branches`. Dry-run by default; apply
/// requires `apply=true` + a `confirm_ids` subset of the dry-run's all_ids +
/// non-empty `audit_reason` (mirrors #806 sweep's double-opt-in). Args: `agent`
/// (default caller; resolves the bound source_repo via binding.json), `base`
/// (default "main"; compare target for clean/squash-merged detection),
/// `min_age_days` (default 90; stale_idle threshold), `apply` (default false),
/// `confirm_ids` + `audit_reason` (required when apply=true; the latter logged to
/// event-log.jsonl per deleted branch).
pub(crate) fn handle_cleanup_merged_branches(
    home: &Path,
    args: &Value,
    instance_name: &str,
) -> Value {
    let agent = args["instance"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    let source_repo = match crate::binding::read(home, agent)
        .and_then(|v| v["source_repo"].as_str().map(std::path::PathBuf::from))
    {
        Some(p) => p,
        None => {
            return json!({
                "error": format!("no binding source_repo for agent '{agent}'"),
                "code": "no_binding_source_repo",
            });
        }
    };
    let base = args["base"].as_str().unwrap_or("main");
    let min_age_days = args["min_age_days"]
        .as_i64()
        .unwrap_or(crate::branch_sweep::STALE_IDLE_DEFAULT_DAYS);
    let apply = args["apply"].as_bool().unwrap_or(false);
    let now = chrono::Utc::now();
    let categories = match crate::branch_sweep::scan(&source_repo, base, min_age_days, now) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "error": format!("branch sweep scan failed: {e}"),
                "code": "scan_failed",
            });
        }
    };
    if !apply {
        return json!({
            "dry_run": true,
            "categories": &categories,
            "candidate_ids": categories.deletable_ids(),
            "total_candidates": categories.total(),
            "to_apply_hint": "repo action=cleanup_merged_branches apply=true confirm_ids=<subset> audit_reason=<...>",
            "active_unknown_note": "active_unknown bucket is NOT in candidate_ids by default — include those names explicitly in confirm_ids if you really want to delete them",
        });
    }
    let confirm_ids: std::collections::HashSet<String> = args["confirm_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if confirm_ids.is_empty() {
        return json!({
            "error": "apply=true requires non-empty 'confirm_ids' (subset of candidate_ids from prior dry-run)",
            "code": "missing_confirm_ids",
        });
    }
    let audit_reason = args["audit_reason"].as_str().unwrap_or("");
    if audit_reason.is_empty() {
        return json!({
            "error": "apply=true requires non-empty 'audit_reason' for the event-log entry",
            "code": "missing_audit_reason",
        });
    }
    // Validate confirm_ids ⊆ all_ids (including active_unknown for
    // explicit opt-in). Mismatch ⇒ reject loudly — operator must
    // re-run dry-run to refresh the candidate list.
    let candidate_set: std::collections::HashSet<String> =
        categories.all_ids().into_iter().collect();
    let unknown: Vec<String> = confirm_ids.difference(&candidate_set).cloned().collect();
    if !unknown.is_empty() {
        return json!({
            "error": "confirm_ids contained entries not in current sweep candidates",
            "unknown": unknown,
            "hint": "re-run dry-run; candidates may have changed since last scan",
            "code": "stale_confirm_ids",
        });
    }
    match crate::branch_sweep::emit_delete_batch(
        home,
        &source_repo,
        &categories,
        &confirm_ids,
        audit_reason,
    ) {
        Ok(count) => json!({
            "applied": count,
            "audit_reason": audit_reason,
            "restore_hint": "see event-log.jsonl `branch_sweep_apply` entries for source SHAs (git branch <name> <sha>)",
        }),
        Err(e) => json!({
            "error": format!("branch sweep apply failed: {e}"),
            "code": "apply_failed",
        }),
    }
}
