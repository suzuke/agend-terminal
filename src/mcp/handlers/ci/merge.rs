use serde_json::{json, Value};
use std::path::Path;

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

    if force && force_reason.is_empty() {
        return json!({"error": "force=true requires non-empty force_reason"});
    }

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
        // pr_view error → fail-OPEN (proceed): GitHub may still be computing
        // mergeability and we must not block a real merge on a transient (the #813
        // mergeable-check pattern). Reuses the same `pr_view` path
        // `verify_merge_landed` uses — no new infra. `force=true` bypasses (the
        // audit block below logs it, like the CI gate).
        if let Ok(summary) =
            crate::scm::make_scm_provider(&repo, None).pr_view(&repo, pr, &["mergeStateStatus"])
        {
            let mss = summary.merge_state_status.as_deref().unwrap_or("");
            if let Some((why, hint)) = base_drift_refusal(mss) {
                return json!({
                    "error": format!("base is stale — merge refused: {why}"),
                    "merge_state_status": mss,
                    "hint": format!("{hint}; or force=true with force_reason for emergency bypass"),
                });
            }
        }
        // #2140: deterministic freshness gate (logic in ci/merge_freshness.rs).
        if let Some(refusal) = super::merge_freshness::gate(&repo, pr) {
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
        let events_path = home.join("fleet_events.jsonl");
        let audit_written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(events_path)?;
            writeln!(f, "{event}")?;
            Ok(())
        })();
        if let Err(e) = audit_written {
            return json!({
                "error": format!("force-merge refused: audit log write failed: {e}"),
                "hint": "fix fleet_events.jsonl permissions or disk space, then retry"
            });
        }
    }

    // #PR-Z site 3: the ONLY write — `gh pr merge` via ScmProvider. argv
    // byte-identical (`pr merge <pr> --repo R --admin --squash
    // --delete-branch`, pinned by scm::tests::pr_merge_args_match_existing_gh_call).
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
        },
    ) {
        // #1467: `gh pr merge` exit 0 is NECESSARY but not SUFFICIENT — a
        // merge-queue / branch-protection / eventual-consistency situation can
        // exit 0 without the PR actually landing (observed: cross-team PRs
        // reported merged:true while still OPEN, commits unpushed). Verify the
        // post-condition with `gh pr view` before claiming success.
        Ok(crate::scm::MergeOutcome::Submitted) => match verify_merge_landed(&repo, pr) {
            MergeVerdict::Confirmed(merge_commit) => json!({
                "merged": true,
                "pr": pr,
                "forced": force,
                "mergeCommit": merge_commit,
            }),
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
