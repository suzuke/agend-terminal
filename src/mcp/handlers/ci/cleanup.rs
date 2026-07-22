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
/// event-log.jsonl per deleted branch). An explicit `repository_path` is accepted
/// for its configured orchestrator and never falls back to an unrelated binding.
pub(crate) fn handle_cleanup_merged_branches(
    home: &Path,
    args: &Value,
    instance_name: &str,
) -> Value {
    let agent = args["instance"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    let source_repo = match resolve_cleanup_source_repo(home, args, agent, instance_name) {
        Ok(path) => path,
        Err(error) => return error,
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
        let (categories_with_observability, spike_residue) =
            match crate::branch_sweep::dry_run_observability(&source_repo, base, &categories) {
                Ok(report) => report,
                Err(e) => {
                    return json!({
                        "error": format!("branch sweep observability failed: {e}"),
                        "code": "scan_failed",
                    });
                }
            };
        return json!({
            "dry_run": true,
            "categories": categories_with_observability,
            "annotations": {
                "spike_residue": spike_residue,
            },
            "candidate_ids": categories.deletable_ids(),
            "total_candidates": categories.total(),
            "to_apply_hint": "repo action=cleanup_merged_branches apply=true confirm_ids=<subset> audit_reason=<...>",
            "active_unknown_note": "active_unknown bucket is not deletable until terminal provenance and preservation evidence are proven; it remains visible for operator follow-up",
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
    // visibility). Mismatch ⇒ reject loudly — operator must re-run dry-run
    // to refresh the candidate list; lifecycle apply remains fail-closed.
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
    match crate::branch_sweep::emit_delete_batch_with_context(
        Some(home),
        &source_repo,
        base,
        &categories,
        &confirm_ids,
        audit_reason,
    ) {
        Ok((count, skipped)) => json!({
            "applied": count,
            "skipped": skipped,
            "audit_reason": audit_reason,
            "restore_hint": "see event-log.jsonl `branch_sweep_apply` entries for source SHAs (git branch <name> <sha>)",
        }),
        Err(e) => json!({
            "error": format!("branch sweep apply failed: {e}"),
            "code": "apply_failed",
        }),
    }
}

/// Resolve the cleanup target without ever falling back from an explicit
/// `repository_path` to a caller binding. An unbound caller is allowed only
/// when it is the configured orchestrator for the canonical source path;
/// operator-direct calls (`instance_name == ""`) retain the existing operator
/// authority.
fn resolve_cleanup_source_repo(
    home: &Path,
    args: &Value,
    agent: &str,
    caller: &str,
) -> Result<std::path::PathBuf, Value> {
    if let Some(raw) = args["repository_path"].as_str().filter(|s| !s.is_empty()) {
        let path = match std::fs::canonicalize(raw) {
            Ok(path) if path.is_dir() => path,
            Ok(path) => {
                return Err(json!({
                    "error": format!("repository_path is not a directory: {}", path.display()),
                    "code": "invalid_repository_path",
                }));
            }
            Err(e) => {
                return Err(json!({
                    "error": format!("repository_path is not readable: {e}"),
                    "code": "invalid_repository_path",
                }));
            }
        };
        let authority = if caller.is_empty() { agent } else { caller };
        if !authority.is_empty() && !orchestrator_owns_repo(home, authority, &path) {
            return Err(json!({
                "error": format!("'{authority}' is not authorized to clean repository_path '{}'", path.display()),
                "code": "repository_path_unauthorized",
            }));
        }
        return Ok(path);
    }

    crate::binding::read(home, agent)
        .and_then(|v| v["source_repo"].as_str().map(std::path::PathBuf::from))
        .ok_or_else(|| {
            json!({
                "error": format!("no binding source_repo for agent '{agent}'"),
                "code": "no_binding_source_repo",
            })
        })
}

fn orchestrator_owns_repo(home: &Path, caller: &str, repo: &Path) -> bool {
    let Ok(repo) = std::fs::canonicalize(repo) else {
        return false;
    };
    crate::teams::list_all(home).into_iter().any(|team| {
        team.orchestrator.as_deref() == Some(caller)
            && team
                .source_repo
                .as_deref()
                .and_then(|source| std::fs::canonicalize(source).ok())
                .is_some_and(|source| source == repo)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_repository_path_requires_its_orchestrator_and_never_falls_back() {
        let root = std::env::temp_dir().join(format!(
            "agend-cleanup-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let home = root.join("home");
        let target = root.join("target-repo");
        let unrelated = root.join("unrelated-repo");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::create_dir_all(&unrelated).expect("unrelated");

        // An unbound non-orchestrator cannot redirect cleanup to an explicit
        // repository, even when its stale binding points at another repo.
        let binding_dir = home.join("runtime").join("worker");
        std::fs::create_dir_all(&binding_dir).expect("binding dir");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({"source_repo": unrelated}).to_string(),
        )
        .expect("binding");
        let args = serde_json::json!({"repository_path": target});
        let denied =
            resolve_cleanup_source_repo(&home, &args, "worker", "worker").expect_err("deny");
        assert_eq!(denied["code"], "repository_path_unauthorized");

        let created = crate::teams::create(
            &home,
            &serde_json::json!({
                "name": "archfix",
                "members": ["orchestrator"],
                "orchestrator": "orchestrator",
                "repository_path": target,
            }),
        );
        assert_eq!(created["status"], "created", "team setup failed: {created}");
        let spoofed = resolve_cleanup_source_repo(
            &home,
            &serde_json::json!({"repository_path": target}),
            "orchestrator",
            "worker",
        )
        .expect_err("target instance must not spoof caller authority");
        assert_eq!(spoofed["code"], "repository_path_unauthorized");
        let resolved = resolve_cleanup_source_repo(
            &home,
            &serde_json::json!({"repository_path": target}),
            "orchestrator",
            "orchestrator",
        )
        .expect("configured orchestrator may target its repo");
        assert_eq!(
            resolved,
            std::fs::canonicalize(&target).expect("target canonicalization")
        );
        std::fs::remove_dir_all(root).ok();
    }
}
