//! MCP handlers for daemon-managed worktree lifecycle. Operator- and
//! agent-callable: `bind_self` (Sprint 54 P1-7), `release_worktree`
//! (Sprint 53 P0-X). Sibling non-destructive GC visibility (Sprint 53 P1-4,
//! wraps `worktree_pool::gc_dry_run`) used to live alongside these as an MCP
//! tool; #2548 moved it to `cli::handle_gc_dry_run` (`agend-terminal admin
//! gc-dry-run`) — no longer part of the MCP surface.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `bind_self` (Sprint 54 P1-7). Lets any instance proactively
/// bind itself to a worktree on the named branch via the daemon's
/// standard lease lifecycle.
///
/// **When to use vs `repo action=checkout bind:true`** (#779 Option 1):
/// - **`bind:true`** — preferred for fresh-task dispatches where the
///   caller already knows the source repo (passes explicit
///   `repository_path` arg). Single-step atomic provision + bind.
/// - **`bind_self`** — preferred for mid-lifecycle scenarios:
///   (a) re-binding a recovered worktree via `rebase_mode=true`,
///   (b) binding via fleet.yaml-resolved source repo (caller has no
///   explicit `repository_path` arg available),
///   (c) post-`release_worktree` re-claim of the same branch.
///
/// Both paths share `dispatch_auto_bind_lease` so binding.json +
/// .agend-managed marker + auto watch_ci all land. Bug fixes in the
/// dispatch path inherit automatically.
///
/// Required args: `repository_path` / `repository` (one of), `branch`.
/// Returns `{bound, worktree_path, branch}` on success or `{error, code}`
/// on failure.
pub(crate) fn handle_bind_self(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let agent = match sender.as_ref().map(Sender::as_str) {
        Some(a) if !a.is_empty() => a,
        _ => {
            return json!({
                "error": "bind_self requires AGEND_INSTANCE_NAME — anonymous callers cannot bind",
                "code": "needs_identity"
            })
        }
    };
    let branch = match args["branch"].as_str() {
        Some(b) if !b.is_empty() => b,
        _ => return json!({"error": "missing 'branch'", "code": "missing_arg"}),
    };
    if !crate::agent_ops::validate_branch(branch) {
        return json!({
            "error": format!("invalid branch name '{branch}'"),
            "code": "invalid_branch"
        });
    }

    // Sprint 55 P0-B EC9: dual-arg shape with two-sprint deprecation cycle.
    // - `repository_path: <local path>` (NEW unified shape; daemon derives owner/repo)
    // - `repository: "owner/name"` (legacy GitHub slug; warn-log; removed Sprint 57)
    // Both present → reject as `ambiguous_args`. Neither → fleet.yaml fallback chain.
    let source_repo_arg = args["repository_path"].as_str().filter(|s| !s.is_empty());
    let repo_arg = args["repository"].as_str().filter(|s| !s.is_empty());
    if source_repo_arg.is_some() && repo_arg.is_some() {
        return json!({
            "error": "both 'repository_path' and 'repository' provided — pass exactly one",
            "code": "ambiguous_args"
        });
    }
    if repo_arg.is_some() {
        tracing::warn!(
            %agent,
            "bind_self(repository=...) is deprecated; use bind_self(repository_path=<local-path>) — Sprint 55 warning, Sprint 57 removal"
        );
    }
    let source_repo_path = source_repo_arg.map(std::path::PathBuf::from);

    // Issue #689: reject path traversal in repository_path
    if let Some(ref p) = source_repo_path {
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return json!({
                "error": "repository_path must not contain '..' (path traversal rejected)",
                "code": "path_traversal"
            });
        }
    }

    // #2496: rebase_mode tries a SAFE same-agent repair FIRST — reads the
    // agent's actual bound worktree and either finds it already on `branch`
    // (metadata-only, no mutation) or in-place `git switch`es a clean
    // worktree to it. Any condition that isn't safe (dirty, not
    // daemon-managed, held by another agent, an active CI watch/task on the
    // branch being abandoned, or the switch itself failing) is a fail-closed
    // BLOCKED error — no more silent fallthrough to a full destructive
    // release (that defeated the entire point of `rebase_mode`).
    let mut repair_action = None;
    if args["rebase_mode"].as_bool().unwrap_or(false) {
        match crate::mcp::handlers::force_release::attempt_safe_rebind_repair(home, agent, branch) {
            Ok(action) => repair_action = Some(action),
            Err(reason) => {
                return json!({
                    "error": format!("rebase_mode repair blocked: {reason}"),
                    "code": "rebind_repair_blocked"
                });
            }
        }
    }

    let task_id = args["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    match crate::mcp::handlers::dispatch_hook::dispatch_auto_bind_lease_with_source(
        home,
        agent,
        task_id,
        branch,
        repo_arg,
        source_repo_path.as_deref(),
    ) {
        Ok(_outcome) => {
            // Successful bind: read back the worktree path from the binding
            // we just wrote so the response reflects authoritative state.
            // #2550 W3 Wave2: `bind_full` updates `binding::binding_index()`
            // under the same lock it writes the file in, so the cache is
            // already current here — `read()` reflects it without a second
            // disk read. #781 `DispatchOutcome` fields are dropped here —
            // surfacing them is a `bind_self` consumer follow-up.
            let worktree_path = crate::binding::read(home, agent)
                .and_then(|v| v["worktree"].as_str().map(String::from))
                .unwrap_or_default();
            let mut resp = json!({
                "bound": true,
                "worktree_path": worktree_path,
                "branch": branch,
            });
            // #2496: surface exactly what the safe repair did (or that it
            // wasn't invoked at all) — the acceptance criteria requires
            // callers to know whether metadata-only, a branch switch, or
            // nothing happened.
            if let Some(action) = repair_action {
                resp["repair_action"] = serde_json::to_value(action).unwrap_or(Value::Null);
            }
            resp
        }
        Err(err) => {
            // Map `DispatchError` to the pre-#781 string-code response shape, but
            // dispatch on the TYPED `err.code` — NOT message substrings
            // (smells#2 Pattern-A / de2eb8 finding #1). The old
            // `msg.contains("already leased")` misclassified the two
            // `LeaseConflict` producers whose message lacks that phrase
            // (lock-acquire failure, `worktree::create` None) as `lease_failed`
            // instead of `cross_agent_conflict`; matching the variant fixes that
            // and consolidates all lease conflicts under one stable code.
            use super::dispatch_hook::ErrorCode;
            let code = match err.code {
                ErrorCode::ProtectedBranch => "e4_5_protected_branch",
                ErrorCode::LeaseConflict => "cross_agent_conflict",
                _ => "lease_failed",
            };
            json!({"error": err.message, "code": code})
        }
    }
}

/// MCP tool: `release_worktree`. Required arg: `instance`. Returns
/// `{released, worktree_removed, binding_removed, error}` —
/// `released:true` clears binding; worktree removal via
/// `git worktree remove --force` (or fallback). Idempotent (#1465) — a
/// second call (no binding) returns `released:true, already_released:true`
/// (success no-op, no error).
///
/// `force:true` (#2548 PR-2) absorbs the former standalone
/// `force_release_worktree` tool — see [`handle_release_worktree_force`].
/// `force:false` (default) is this original binding-driven path,
/// byte-identical to pre-#2548 behavior.
pub(crate) fn handle_release_worktree(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let agent = match args["instance"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'instance'"}),
    };

    if args["force"].as_bool().unwrap_or(false) {
        return handle_release_worktree_force(home, args, agent, sender);
    }

    crate::validate_name_or_err!(agent);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    // #789: clean empty init commits before removal (best-effort).
    if !dry_run {
        if let Some(wt) = crate::binding::read(home, agent)
            .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
        {
            let _ = crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&wt).ok();
        }
    }
    let outcome = crate::worktree_pool::release_full(home, agent, dry_run);
    serde_json::to_value(&outcome).unwrap_or_else(|_| json!({"error": "serialize failed"}))
}

/// `release_worktree(force:true)` (#2548 PR-2) — absorbed from the former
/// standalone `force_release_worktree` MCP tool. Cleans a stale daemon-
/// managed worktree directory directly via
/// `<home>/worktrees/<agent>/<branch>/`, bypassing the `.agend-managed`
/// marker check the `force:false` path enforces — intentional: this path
/// exists specifically for stale-state recovery where a directory survives
/// after its binding is already gone (see `rebase_clean_self`'s docs), and
/// the marker check would block exactly that recovery. Requires `branch`
/// (the `force:false` path derives it from the binding; here there may be
/// no binding left to read).
///
/// AUDIT2-002: since this path deletes disk state without the marker safety
/// net, restrict callers to the target's own agent or its team orchestrator.
/// An anonymous caller (operator-direct) keeps full authority.
fn handle_release_worktree_force(
    home: &Path,
    args: &Value,
    agent: &str,
    sender: &Option<Sender>,
) -> Value {
    if let Err(e) = crate::agent::validate_name(agent) {
        return json!({"error": e, "code": "invalid_agent"});
    }
    let branch = match args["branch"].as_str() {
        Some(b) if !b.is_empty() => b,
        _ => return json!({"error": "missing 'branch'"}),
    };
    if let Some(caller) = sender.as_ref().map(|s| s.as_str()) {
        if caller != agent && !crate::teams::is_orchestrator_of(home, caller, agent) {
            return json!({
                "error": format!(
                    "permission denied: '{caller}' cannot force-release '{agent}'s worktree \
                     (only the owner or its team orchestrator may)"
                ),
                "code": "not_owner_or_orchestrator"
            });
        }
    }
    if !crate::agent_ops::validate_branch(branch) {
        return json!({
            "error": format!("invalid branch name '{branch}'"),
            "code": "invalid_branch"
        });
    }

    // Safety: ensure the resolved target is within the worktrees pool AND
    // deeper than the agent-level subdirectory (a `branch == ""` would
    // otherwise resolve to the agent's own dir; the empty-string check
    // above already rejects this, but the defense-in-depth guard catches
    // future validator drift).
    let worktrees_root = home.join("worktrees");
    let target = worktrees_root.join(agent).join(branch);
    let safe = target.starts_with(&worktrees_root)
        && target != worktrees_root
        && target != worktrees_root.join(agent);
    if !safe {
        return json!({
            "error": format!(
                "release_worktree(force:true) refuses to clean path outside the daemon \
                 worktree pool: {}",
                target.display()
            ),
            "code": "path_outside_pool"
        });
    }

    // #826: optional operator-supplied `repository_path` arg. When present,
    // L2 GC skips enumeration and goes straight to the named repo.
    let source_repo_hint = args["repository_path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);

    match crate::mcp::handlers::force_release::rebase_clean_self(
        home,
        agent,
        branch,
        source_repo_hint.as_deref(),
        sender.as_ref().map(|s| s.as_str()),
    ) {
        Ok(o) => {
            if let Some(error) = o.binding_outcome["error"].as_str() {
                return json!({
                    "released": false,
                    "dir_existed": o.dir_existed,
                    "dir_removed": o.dir_removed,
                    "binding_outcome": o.binding_outcome,
                    "error": error,
                    "code": "force_release_refused",
                    "git_metadata_pruned": 0,
                    "git_metadata_repos": [],
                });
            }
            // #826 L2 GC: when the binding-clear path short-circuited on
            // "no binding" (the post-disband state), the
            // `git worktree remove --force` step inside `release_full`
            // never ran. Run it now against any source repos that still
            // hold `.git/worktrees/<meta-dir>/` metadata for our target
            // worktree path.
            let gc = crate::mcp::handlers::force_release::prune_git_metadata_for_agent(
                home,
                agent,
                branch,
                source_repo_hint.as_deref(),
            );
            json!({
                "released": true,
                "dir_existed": o.dir_existed,
                "dir_removed": o.dir_removed,
                "binding_outcome": o.binding_outcome,
                "git_metadata_pruned": gc.pruned_count,
                "git_metadata_repos": gc.repos_touched,
            })
        }
        Err(e) => json!({"error": e, "code": "force_release_refused"}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod path_traversal_tests;
