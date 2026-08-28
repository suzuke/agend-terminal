use super::watch::handle_watch_ci;
use serde_json::{json, Value};
use std::path::Path;

/// Post-merge receipt persistence + actionable exact-head watch auto-arm.
/// Separated for testability — the merge handler calls this after
/// `MergeVerdict::Confirmed`. Returns diagnostic fields to embed in the
/// merge response. Merge success is truthful regardless of this outcome.
///
/// `pr_branch`: the PR's source branch (headRefName). Used to find the
/// task assignee whose binding matches this branch — NOT the merge caller,
/// because the merge caller is typically the orchestrator (unbound).
///
/// `task_id`: optional explicit task-board id passed by the merge caller
/// (`repo(action=merge, ..., task_id)`). When present it SHORT-CIRCUITS the
/// binding scan entirely: zero search, zero ambiguity — the task's own
/// `assignee` is read via a single strict-routed lookup, and a missing /
/// terminal / assignee-less task fails closed with a reason. Absent → the
/// legacy stage-1 live-binding scan runs unchanged.
pub(crate) fn post_merge_receipt_and_watch(
    home: &Path,
    repo: &str,
    merge_commit: &str,
    pr: u64,
    pr_branch: &str,
    merge_authority: &str,
    task_id: Option<&str>,
) -> Value {
    let (assignee, task_id) = match resolve_task_assignee(home, repo, pr_branch, task_id) {
        Ok(pair) => pair,
        // The Err already carries the SPECIFIC reason (explicit-id failures name
        // the task and the failed check; the legacy scan keeps its generic
        // binding wording) — surface it verbatim so the operator sees the cause
        // without digging through the daemon log.
        Err(reason) => return json!({"skipped": reason}),
    };
    let expiry =
        chrono::Utc::now() + chrono::TimeDelta::try_hours(1).unwrap_or(chrono::TimeDelta::zero());
    let receipt = crate::merge_receipt::MergeReceipt {
        repo: repo.to_string(),
        merge_sha: merge_commit.to_string(),
        task_id: task_id.clone(),
        task_assignee: assignee.clone(),
        merge_authority: merge_authority.to_string(),
        pr_number: pr,
        created_at: chrono::Utc::now().to_rfc3339(),
        expires_at: expiry.to_rfc3339(),
    };
    if let Err(e) = crate::merge_receipt::persist(home, &receipt) {
        return json!({"receipt_error": format!("receipt persist failed: {e}")});
    }
    let next_after_ci = if merge_authority.is_empty() {
        assignee.as_str()
    } else {
        merge_authority
    };
    let watch_result = handle_watch_ci(
        home,
        &json!({
            "repository": repo,
            "branch": "main",
            "head_sha": merge_commit,
            "task_id": &task_id,
            "next_after_ci": [next_after_ci],
        }),
        "",
    );
    let has_subscribers = watch_result
        .get("subscribers")
        .and_then(Value::as_array)
        .is_some_and(|subscribers| !subscribers.is_empty());
    if watch_result.get("error").is_some() || !has_subscribers {
        let watch_error = watch_result
            .get("error")
            .cloned()
            .unwrap_or_else(|| json!("watch armed without subscribers"));
        json!({
            "receipt": "persisted",
            "assignee": &assignee,
            "watch_error": watch_error,
        })
    } else {
        json!({
            "receipt": "persisted",
            "assignee": &assignee,
            "watch": "armed",
        })
    }
}

/// Two-stage resolve entry: an explicit `task_id` (from the merge caller)
/// short-circuits to a single strict-routed task lookup; `None` falls through
/// to the legacy live-binding scan, byte-identical to main. Parameter order
/// `(home, repo, branch, task_id)` matches `resolve_task_assignee_for_branch`
/// so the two adjacent `&str`s can't be transposed at call sites.
///
/// On failure returns `Err(reason)` — for the explicit-id path that reason is
/// SPECIFIC (names the task and the failed check) and is surfaced verbatim in
/// the MCP response; a caller passing `task_id` and getting skipped has a bug,
/// and #3341's original symptom was exactly this skip being opaque. The
/// legacy-scan path keeps its single generic reason.
fn resolve_task_assignee(
    home: &Path,
    repo: &str,
    branch: &str,
    task_id: Option<&str>,
) -> Result<(String, String), String> {
    if let Some(id) = task_id.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_task_assignee_by_id(home, id, branch);
    }
    resolve_task_assignee_for_branch(home, repo, branch)
        .ok_or_else(|| "no task-linked binding for PR branch".to_string())
}

/// Direct lookup path: read the task by id via the strict router
/// (`tasks::load_routed` — fail-closed on duplicate/unreadable boards) and
/// return `(assignee, task_id)`. The passed-in id is VERIFIED, not trusted:
/// the task's own `branch` must equal the branch being merged, else Err —
/// a mismatched hint means the CALLER is buggy and silently switching to the
/// binding scan would only hide that. Every failure returns a SPECIFIC reason
/// (Err) so the MCP response can show the operator which check failed.
///
/// Catalog migration registry (DESIGN-task-catalog-projection Appendix A):
/// this `load_routed` call site converts 1:1 to `catalog.route(task_id)` at
/// the P2 authority cutover — same fail-closed error set
/// (NotFound/Ambiguous/Unreadable), signature already matches.
fn resolve_task_assignee_by_id(
    home: &Path,
    task_id: &str,
    branch: &str,
) -> Result<(String, String), String> {
    let routed = crate::tasks::load_routed(home, task_id)
        .map_err(|_| format!("task_id '{task_id}' not found"))?;
    let task = routed.task;
    if task.branch.as_deref() != Some(branch) {
        tracing::warn!(%task_id, %branch, task_branch = ?task.branch,
            "post-merge watch resolve: task_id does not name the branch being merged");
        return Err(format!(
            "task_id '{task_id}' names branch '{}', not '{branch}'",
            task.branch.as_deref().unwrap_or("<none>")
        ));
    }
    if task.status.is_terminal() {
        tracing::warn!(%task_id, status = %task.status, "post-merge watch resolve: task is terminal");
        return Err(format!("task_id '{task_id}' is terminal ({})", task.status));
    }
    let assignee = task
        .assignee
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("task_id '{task_id}' has no assignee"))?;
    Ok((assignee, task_id.to_string()))
}

/// Resolve the task assignee for a PR branch by scanning all bindings.
/// Returns `(agent_name, task_id)` or `None` if no unique match.
/// Fail-closed: ambiguity (multiple matches), no match, or repo
/// mismatch → None. Requires canonical repo + branch + non-empty task_id.
fn resolve_task_assignee_for_branch(
    home: &Path,
    repo: &str,
    branch: &str,
) -> Option<(String, String)> {
    if branch.is_empty() || repo.is_empty() {
        return None;
    }
    let repo_lower = repo.to_lowercase();
    let bindings = crate::binding::binding_scan_all(home);
    let mut matches: Vec<(String, String)> = Vec::new();
    for (agent, binding) in &bindings {
        let b_branch = binding["branch"].as_str().unwrap_or("");
        let b_task = binding["task_id"].as_str().unwrap_or("");
        if b_branch != branch || b_task.is_empty() {
            continue;
        }
        let b_source = binding["source_repo"].as_str().unwrap_or("");
        if b_source.is_empty() {
            continue;
        }
        let b_slug = crate::mcp::handlers::dispatch_hook::canonical_repo_slug_for_source(
            std::path::Path::new(b_source),
        );
        let repo_matches = b_slug
            .as_deref()
            .is_some_and(|s| s.to_lowercase() == repo_lower);
        if repo_matches {
            matches.push((agent.clone(), b_task.to_string()));
        }
    }
    if matches.len() == 1 {
        Some(matches.remove(0))
    } else {
        None
    }
}

/// #1467: outcome of post-merge verification via `gh pr view`.
pub(crate) enum MergeVerdict {
    /// PR confirmed merged: `state == "MERGED"` AND a non-empty merge commit
    /// oid. Carries the merge commit SHA.
    Confirmed(String),
    /// Not (yet) confirmed merged. May be transient (merge queue / eventual
    /// consistency) — caller should re-query, not treat as a hard failure.
    Unconfirmed {
        state: String,
        merge_state_status: String,
    },
}

/// #1467: classify a `gh pr view` result into a [`MergeVerdict`]. PURE —
/// tests drive it directly without shelling `gh`. A PR is confirmed merged
/// only when GitHub reports `state == "MERGED"` AND a non-empty merge-commit
/// oid. #PR-D: takes the typed [`crate::scm::PrSummary`] (was a raw `Value`);
/// the three reads map 1:1 (`state` → `state`; `mergeCommit.oid` →
/// `merge_commit_oid`, empty→None; `mergeStateStatus` → `merge_state_status`),
/// so the verdict is byte-for-byte the same.
pub(crate) fn classify_merge_summary(s: &crate::scm::PrSummary) -> MergeVerdict {
    let state = s.state.clone().unwrap_or_else(|| "UNKNOWN".to_string());
    let oid = s.merge_commit_oid.clone().unwrap_or_default();
    if state == "MERGED" && !oid.is_empty() {
        MergeVerdict::Confirmed(oid)
    } else {
        MergeVerdict::Unconfirmed {
            state,
            merge_state_status: s.merge_state_status.clone().unwrap_or_default(),
        }
    }
}

/// #1467: after `gh pr merge` reports success, confirm the PR actually landed.
/// Bounded poll (≤3 attempts, 2s apart) to tolerate merge-queue / eventual-
/// consistency lag — NOT an infinite wait. Returns the last verdict seen; the
/// first `Confirmed` short-circuits.
fn verify_merge_landed(repo: &str, pr: u64) -> MergeVerdict {
    // #PR-D site 1: the single `gh pr view` goes through ScmProvider. argv
    // byte-identical (`pr view <pr> --repo R --json state,mergeCommit,
    // mergedAt,mergeStateStatus`). The retry loop stays here (deliberately
    // NOT folded into the trait — spike §4). On any gh failure pr_view
    // returns Err → keep polling / fall back to `last` (was the prior
    // non-success / parse-fail skip).
    let provider = crate::scm::make_scm_provider(repo, None);
    let mut last = MergeVerdict::Unconfirmed {
        state: "UNKNOWN".to_string(),
        merge_state_status: String::new(),
    };
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if let Ok(summary) = provider.pr_view(
            repo,
            pr,
            &["state", "mergeCommit", "mergedAt", "mergeStateStatus"],
        ) {
            match classify_merge_summary(&summary) {
                MergeVerdict::Confirmed(c) => return MergeVerdict::Confirmed(c),
                unconfirmed => last = unconfirmed,
            }
        }
    }
    last
}

/// #base-drift: pure decision — should GitHub's `mergeStateStatus` REFUSE the
/// merge? `BEHIND` (PR base behind main → an `--admin` squash lands a
/// phantom-reversion diff, dev-2 #1798) and `DIRTY` (conflicts) refuse;
/// everything else (CLEAN / UNSTABLE / BLOCKED / UNKNOWN / empty) proceeds —
/// fail-OPEN, because GitHub may still be computing mergeability and we must not
/// block a real merge on a transient (#813 pattern). Returns `Some((why, hint))`
/// to refuse, `None` to proceed.
pub(crate) fn base_drift_refusal(merge_state_status: &str) -> Option<(&'static str, &'static str)> {
    match merge_state_status {
        "BEHIND" => Some((
            "PR base is behind main (phantom-reversion risk)",
            "rebase onto current main: git fetch && git rebase origin/main && git push --force-with-lease",
        )),
        "DIRTY" => Some((
            "PR has merge conflicts with main",
            "resolve: git fetch && git rebase origin/main, fix conflicts, git push --force-with-lease",
        )),
        _ => None,
    }
}

/// P0 exact-head: read the PR's current `(head, base)` OIDs and pre-merge
/// metadata in one `gh pr view`. Returns `None` if either OID is missing or the
/// read errors — the caller MUST fail closed (a merge that cannot identify its
/// exact head+base is unsafe, even under `force`). `base_ref_oid` is the base
/// BRANCH's current tip (it advances as the base moves), so comparing it
/// gate-vs-pre-merge detects a base advance by EXACT identity —
/// `mergeStateStatus` is derived + laggy and cannot prove base identity.
/// `include_merge_state` is true only for the initial policy snapshot; the
/// exact identity recheck intentionally requests identity fields only.
fn acquire_head_base(
    repo: &str,
    pr: u64,
    include_merge_state: bool,
) -> Option<(String, String, String, Option<String>)> {
    let provider = crate::scm::make_scm_provider(repo, None);
    let s = if include_merge_state {
        provider
            .pr_view(
                repo,
                pr,
                &[
                    "headRefOid",
                    "baseRefOid",
                    "headRefName",
                    "mergeStateStatus",
                ],
            )
            .ok()?
    } else {
        provider
            .pr_view(repo, pr, &["headRefOid", "baseRefOid", "headRefName"])
            .ok()?
    };
    let head = s
        .head_ref_oid
        .filter(|x| crate::daemon::ci_watch::is_full_commit_sha(x))?;
    let base = s
        .base_ref_oid
        .filter(|x| crate::daemon::ci_watch::is_full_commit_sha(x))?;
    let branch = s.head_ref.unwrap_or_default();
    Some((head, base, branch, s.merge_state_status))
}

pub(crate) fn handle_merge_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let pr = match args["pr"].as_u64() {
        Some(n) => n,
        None => return json!({"error": "missing 'pr' (PR number)"}),
    };
    // #1619: resolve via the shared helper instead of the old
    // `.unwrap_or("suzuke/agend-terminal")` — a detection miss must NOT
    // silently merge/check/state-query against the maintainer's repo.
    let repo = match super::resolve_repo_or_error(home, instance_name, args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let force = args["force"].as_bool().unwrap_or(false);
    let force_reason = args["force_reason"].as_str().unwrap_or("");
    // Task-id passthrough: an explicit task-board id lets the merge caller
    // hand the post-merge watch its linkage directly — zero binding/board
    // search, zero ambiguity. Optional; absent → legacy resolve unchanged.
    let explicit_task_id = args["task_id"].as_str();

    if force && force_reason.is_empty() {
        return json!({"error": "force=true requires non-empty force_reason"});
    }

    // P0 exact-head precondition: acquire the exact (head, base) OIDs this merge
    // will pin/validate against. NON-BYPASSABLE — even a force merge must land the
    // head+base it INTENDS; if either can't be read, fail closed (never merge a
    // head/base we cannot identify). `force` relaxes only the CI/verdict/freshness
    // POLICY below, never this acquisition nor the pre-merge identity recheck.
    let (gated_head, gated_base, pr_branch, gated_merge_state) = match acquire_head_base(
        &repo, pr, true,
    ) {
        Some(hb) => hb,
        None => {
            return json!({
                "error": "cannot determine PR head+base commit — merge refused (fail-closed exact-head precondition)",
                "hint": "reading the current head+base OIDs is required to pin the merge; retry when the provider (GitHub) is reachable. force does NOT bypass this.",
                "code": "exact_head_unavailable",
            });
        }
    };

    if !force {
        // #PR-D site 2: `gh pr checks` via ScmProvider. argv byte-identical
        // (`pr checks <pr> --repo R --json name,state`). The client-side
        // filter (state ≠ SUCCESS/SKIPPED) reproduces the prior inline one;
        // a null/empty state counts as failing (lenient parse_checks) — same
        // as the prior `as_str().unwrap_or("")`, preserving the fail-closed
        // gate. Intentional observable delta: the prior code surfaced two
        // distinct errors (parse-fail vs query-fail) which pr_checks can't
        // tell apart — both now collapse to ONE fail-closed message. The
        // merge DECISION (any checks problem → refuse) is unchanged.
        let checks = match crate::scm::make_scm_provider(&repo, None).pr_checks(&repo, pr) {
            Ok(c) => c,
            Err(_) => {
                return json!({
                    "error": "CI checks could not be determined — merge refused",
                    "hint": "Verify PR number and repo, or use force=true with force_reason (fail-closed)"
                });
            }
        };
        let failing: Vec<&crate::scm::CheckState> = checks
            .iter()
            .filter(|c| c.state != "SUCCESS" && c.state != "SKIPPED")
            .collect();
        if !failing.is_empty() {
            let summary: Vec<String> = failing
                .iter()
                .map(|c| {
                    // Preserve the prior `unwrap_or("?")` placeholder for an
                    // empty/null name or state.
                    let name = if c.name.is_empty() {
                        "?"
                    } else {
                        c.name.as_str()
                    };
                    let state = if c.state.is_empty() {
                        "?"
                    } else {
                        c.state.as_str()
                    };
                    format!("{name}: {state}")
                })
                .collect();
            return json!({
                "error": "CI checks not all passed — merge refused",
                "failing_checks": summary,
                "hint": "Wait for CI to pass, or use force=true with force_reason for emergency bypass"
            });
        }

        // #base-drift: refuse a stacked/behind PR. GitHub's `mergeStateStatus`
        // BEHIND means the PR base is behind main (another PR merged first) → an
        // `--admin` squash lands a phantom-reversion diff (looks like reverting the
        // already-merged PR — dev-2 #1798, only caught by a manual diff-check +
        // rebase). DIRTY = conflicts (can't merge cleanly). Critically, the
        // `--admin` merge BYPASSES branch-protection's
        // `required_status_checks.strict`, so GitHub will NOT block these — the
        // daemon must. Any other state (CLEAN/UNSTABLE/BLOCKED/UNKNOWN) or a
        // A missing merge-state field remains fail-OPEN (proceed): GitHub may
        // still be computing mergeability and we must not block a real merge on
        // a transient (#813 mergeable-check pattern). The initial exact-head
        // metadata snapshot supplies this field, avoiding a redundant query.
        if let Some(mss) = gated_merge_state.as_deref() {
            if let Some((why, hint)) = base_drift_refusal(mss) {
                return json!({
                    "error": format!("base is stale — merge refused: {why}"),
                    "merge_state_status": mss,
                    "hint": format!("{hint}; or force=true with force_reason for emergency bypass"),
                });
            }
        }
        // #2140: deterministic freshness gate (logic in ci/merge_freshness.rs).
        if let Some(refusal) = super::merge_freshness::gate(&repo, &gated_head) {
            return refusal;
        }
    }

    if force {
        let event = serde_json::json!({
            "kind": "merge_force_bypass",
            "agent": instance_name,
            "pr": pr,
            "repo": repo,
            "force_reason": force_reason,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        // #3416: goes through the one serialized appender. This is a destructive
        // fail-closed gate, so it uses bounded retry and then refuses — never an
        // unlocked fallback. `Err` means no TRUSTWORTHY record exists, which is why
        // the bypass must not proceed: previously an interleaved-but-`Ok` write let
        // this gate report a force-merge as audited while the record on disk was
        // unparseable. Note `Err` does not always mean nothing was written — a
        // `Write` failure can leave a partial line — so the message below reports
        // the error's own wording rather than asserting the file is untouched.
        if let Err(e) = agentic_audit_append::append_audit_line_bounded(
            home,
            &event,
            agentic_audit_append::DEFAULT_BOUNDED_BUDGET,
        ) {
            return json!({
                "error": format!("force-merge refused: {e}"),
                "hint": "another writer holds the audit lock, or the audit log is unwritable (permissions/disk); retry"
            });
        }
    }

    // P0 exact-head: one-shot immediate recheck (NO in-call retry loop). Re-read
    // the current (head, base) and refuse if EITHER moved since the gate — the
    // merge must land the exact head+base that passed validation. Non-bypassable
    // (incl force). A residual ε remains between this read and the write: the HEAD
    // side is additionally pinned at the GitHub API via `--match-head-commit`
    // (below); the BASE side has NO GitHub pin primitive, so its ε is a documented
    // residual — true base atomicity awaits a merge-queue (separate spike). Note:
    // `verify_merge_landed` proves only that the PR LANDED, not that the merge was
    // free of a semantic phantom-reversion; it is NOT a base-race backstop.
    let (head_now, base_now, _, _) = match acquire_head_base(&repo, pr, false) {
        Some(hb) => hb,
        None => {
            return json!({
                "error": "cannot re-read PR head+base before merge — merge refused (fail-closed)",
                "hint": "retry when the provider (GitHub) is reachable; no merge was attempted.",
                "code": "exact_head_recheck_unavailable",
            });
        }
    };
    if head_now != gated_head {
        return json!({
            "error": "PR head moved during merge preparation — merge refused (exact-head)",
            "gated_head": gated_head,
            "current_head": head_now,
            "hint": "re-run the merge; the exact-head precondition intentionally refuses a moved head. No changes were made.",
            "code": "exact_head_moved",
        });
    }
    if base_now != gated_base {
        return json!({
            "error": "base branch advanced during merge preparation — merge refused (exact base identity)",
            "gated_base": gated_base,
            "current_base": base_now,
            "hint": "rebase onto current main and re-run: git fetch && git rebase origin/main && git push --force-with-lease. (Residual: a base advance within the recheck→merge window is uncovered — no GitHub base-pin primitive; true base atomicity awaits a merge-queue.)",
            "code": "exact_base_moved",
        });
    }

    // #PR-Z site 3: the ONLY write — `gh pr merge` via ScmProvider. argv now adds
    // `--match-head-commit <gated_head>` (P0 pin) to the byte-identical
    // `pr merge <pr> --repo R --admin --squash --delete-branch`.
    // MergeOutcome maps the original exit-status branches 1:1: Submitted =
    // exit-0 (→ verify_merge_landed post-condition, unchanged; retry loop
    // stays in that caller), Failed = non-zero (→ "gh pr merge failed" +
    // raw stderr), Err = spawn failure (→ "failed to run gh: {e}").
    match crate::scm::make_scm_provider(&repo, None).pr_merge(
        &repo,
        pr,
        &crate::scm::MergeOpts {
            admin: true,
            squash: true,
            delete_branch: true,
            // P0 pin: fail the merge at the API if the head moved in the ε window.
            expected_head_sha: Some(gated_head.clone()),
        },
    ) {
        // #1467: `gh pr merge` exit 0 is NECESSARY but not SUFFICIENT — a
        // merge-queue / branch-protection / eventual-consistency situation can
        // exit 0 without the PR actually landing (observed: cross-team PRs
        // reported merged:true while still OPEN, commits unpushed). Verify the
        // post-condition with `gh pr view` before claiming success.
        Ok(crate::scm::MergeOutcome::Submitted) => match verify_merge_landed(&repo, pr) {
            MergeVerdict::Confirmed(merge_commit) => {
                let mut resp = json!({
                    "merged": true,
                    "pr": pr,
                    "forced": force,
                    "mergeCommit": &merge_commit,
                });
                let diag = post_merge_receipt_and_watch(
                    home,
                    &repo,
                    &merge_commit,
                    pr,
                    &pr_branch,
                    instance_name,
                    explicit_task_id,
                );
                if let Some(obj) = resp.as_object_mut() {
                    obj.insert("post_merge".into(), diag);
                }
                resp
            }
            MergeVerdict::Unconfirmed {
                state,
                merge_state_status,
            } => json!({
                // NOT merged, but NOT a hard error either: `gh pr merge`
                // succeeded and the PR may still land (merge queue / eventual
                // consistency). Report the true state so the caller can re-query
                // rather than trust a false merged:true.
                "merged": false,
                "pending": true,
                "code": "merge_unconfirmed",
                "pr": pr,
                "state": state,
                "mergeStateStatus": merge_state_status,
                "hint": "gh pr merge reported success but the PR is not yet confirmed MERGED \
                         (possible merge-queue / eventual consistency). Re-query `gh pr view` \
                         before acting; do NOT blindly re-merge.",
            }),
        },
        Ok(crate::scm::MergeOutcome::Failed { stderr }) => {
            json!({
                "error": "gh pr merge failed",
                "stderr": stderr,
            })
        }
        // pr_merge's spawn-failure Err already carries "failed to run gh: …"
        // (set in GitHubScmProvider::run), so surface it as-is — using
        // `e.to_string()` reproduces the original `format!("failed to run
        // gh: {e}")` exactly (no double prefix).
        Err(e) => json!({"error": e.to_string()}),
    }
}
