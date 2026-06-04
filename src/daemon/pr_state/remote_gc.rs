//! #1750-B4: remote-orphan branch GC.
//!
//! When a PR is merged WITHOUT `--delete-branch` (e.g. a GitHub web-UI merge, or
//! a pre-existing orphan), its head branch lingers on the remote forever — the
//! operator counted 16 such stale remote branches. The daemon's own merge path
//! already passes `--delete-branch` (`handle_merge_repo`), so this closes the
//! gap for the merges it didn't perform.
//!
//! Piggybacks on the existing `gh-poll` tick (`scanner::apply_gh_poll`): that
//! poll already returns every PR's `{state, head_ref, merged_at}` via
//! `gh pr list --state all`, so no second poller is needed. After the poll, a
//! merged PR whose head branch STILL exists on the remote (and clears the safety
//! belts) gets its ref deleted via `gh api -X DELETE`.
//!
//! Safety belts (all): delete ONLY a `MERGED` PR's head branch, skip the
//! default/protected branches, skip a head_ref that currently backs an OPEN PR
//! (same-name reuse), and require `merged_at` ≥ [`REMOTE_ORPHAN_MIN_AGE`] (a
//! 3-day human-follow-up buffer — more conservative than B3's local 24h, since
//! deleting a shared remote ref is outward-facing). Low irreversibility: the
//! work is on `main` and the ref is reconstructible from the merge commit.

use super::gh_poll::{GhPrMetadata, GhPrState};
use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

/// #1750-B4: minimum age since `merged_at` before a remote orphan is GC'd.
const REMOTE_ORPHAN_MIN_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// #1750-B4 (pure, unit-tested): from a repo's poll snapshot, pick the remote
/// branches to delete. Deletable iff a PR with that `head_ref` is `MERGED` at
/// least `min_age` ago, the branch still EXISTS on the remote, it is neither the
/// default branch nor protected, and NO open PR currently uses that head_ref.
pub(crate) fn select_remote_orphans_to_delete(
    prs: &[GhPrMetadata],
    existing_branches: &HashSet<String>,
    protected: &HashSet<String>,
    default_branch: &str,
    now: chrono::DateTime<chrono::Utc>,
    min_age: Duration,
) -> Vec<String> {
    // head_refs currently backing an OPEN PR — never delete (name reuse guard).
    let open_refs: HashSet<&str> = prs
        .iter()
        .filter(|p| p.state == GhPrState::Open)
        .map(|p| p.head_ref.as_str())
        .collect();
    let min_age = chrono::Duration::from_std(min_age).unwrap_or_else(|_| chrono::Duration::days(3));

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for pr in prs {
        if pr.state != GhPrState::Merged {
            continue;
        }
        let head = pr.head_ref.as_str();
        if head == default_branch || head == "main" || head == "master" {
            continue;
        }
        if open_refs.contains(head) || protected.contains(head) {
            continue;
        }
        if !existing_branches.contains(head) {
            continue; // merge already deleted it (the common case)
        }
        if seen.contains(head) {
            continue; // a branch can back more than one merged PR over time
        }
        let Some(merged_at) = pr.merged_at.as_deref() else {
            continue;
        };
        let Ok(merged_t) = chrono::DateTime::parse_from_rfc3339(merged_at) else {
            continue;
        };
        if now.signed_duration_since(merged_t.with_timezone(&chrono::Utc)) < min_age {
            continue; // too recent — leave a follow-up buffer
        }
        seen.insert(head);
        out.push(head.to_string());
    }
    out
}

/// #1750-B4: GC remote-orphan branches for `repo` (owner/repo) using the PR
/// snapshot the gh-poll tick already fetched. Best-effort: every gh failure
/// logs and is swallowed — branch GC must never disrupt the scanner.
pub(crate) fn gc_remote_orphans(repo: &str, prs: &[GhPrMetadata]) {
    // Cheap pre-filter: skip the (paginated) branches-list call entirely unless
    // some merged PR is already old enough to possibly GC.
    let now = chrono::Utc::now();
    let min_age = chrono::Duration::from_std(REMOTE_ORPHAN_MIN_AGE)
        .unwrap_or_else(|_| chrono::Duration::days(3));
    let any_candidate = prs.iter().any(|p| {
        p.state == GhPrState::Merged
            && p.merged_at
                .as_deref()
                .and_then(|m| chrono::DateTime::parse_from_rfc3339(m).ok())
                .is_some_and(|t| {
                    now.signed_duration_since(t.with_timezone(&chrono::Utc)) >= min_age
                })
    });
    if !any_candidate {
        return;
    }

    let (existing, protected) = match list_remote_branches(repo) {
        Some(b) => b,
        None => return, // couldn't enumerate — skip rather than blind-delete
    };
    let default = default_branch(repo).unwrap_or_else(|| "main".to_string());

    let to_delete = select_remote_orphans_to_delete(
        prs,
        &existing,
        &protected,
        &default,
        now,
        REMOTE_ORPHAN_MIN_AGE,
    );
    for branch in to_delete {
        if delete_remote_ref(repo, &branch) {
            tracing::info!(
                repo = %repo,
                branch = %branch,
                "#1750-B4: deleted remote-orphan branch (PR merged, branch undeleted)"
            );
        }
    }
}

/// `gh api repos/{repo}/branches --paginate` → ({existing names}, {protected names}).
fn list_remote_branches(repo: &str) -> Option<(HashSet<String>, HashSet<String>)> {
    let out = Command::new("gh")
        .args([
            "api",
            "--paginate",
            &format!("repos/{repo}/branches"),
            "--jq",
            r#".[] | [.name, (.protected|tostring)] | @tsv"#,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::warn!(repo = %repo, "#1750-B4: gh branches list failed — skipping remote GC this tick");
        return None;
    }
    let mut existing = HashSet::new();
    let mut protected = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split('\t');
        let Some(name) = parts.next() else { continue };
        if name.is_empty() {
            continue;
        }
        existing.insert(name.to_string());
        if parts.next() == Some("true") {
            protected.insert(name.to_string());
        }
    }
    Some((existing, protected))
}

/// Resolve the repo's default branch via `gh`.
fn default_branch(repo: &str) -> Option<String> {
    let out = Command::new("gh")
        .args([
            "repo",
            "view",
            repo,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `gh api -X DELETE repos/{repo}/git/refs/heads/{branch}`. Returns true on a
/// successful delete; a 404/422 (already gone) or any error logs and returns
/// false (no retry — the next tick re-evaluates).
fn delete_remote_ref(repo: &str, branch: &str) -> bool {
    let out = Command::new("gh")
        .args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{repo}/git/refs/heads/{branch}"),
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "#1750-B4: remote ref delete failed (left for next tick)"
            );
            false
        }
        Err(e) => {
            tracing::warn!(repo = %repo, branch = %branch, error = %e, "#1750-B4: gh delete spawn failed");
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn pr(number: u64, head: &str, state: GhPrState, merged_at: Option<&str>) -> GhPrMetadata {
        GhPrMetadata {
            number,
            author_login: "x".into(),
            head_ref: head.into(),
            is_draft: false,
            state,
            merged_at: merged_at.map(String::from),
        }
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// #1750-B4: a merged PR whose branch still exists and merged ≥3d ago is
    /// selected; a too-recent merge, an open-PR head_ref, the default/protected
    /// branch, and an already-deleted branch are all skipped.
    #[test]
    fn select_remote_orphans_belts_1750_b4() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-05T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old = "2026-06-01T00:00:00Z"; // 4 days ago → past the 3d floor
        let recent = "2026-06-04T18:00:00Z"; // 6h ago → under the floor
        let prs = vec![
            pr(1, "feat/merged-old", GhPrState::Merged, Some(old)), // → delete
            pr(2, "feat/merged-new", GhPrState::Merged, Some(recent)), // too recent
            pr(3, "feat/open-reuse", GhPrState::Merged, Some(old)), // but also OPEN below
            pr(4, "feat/open-reuse", GhPrState::Open, None),        // open → skip #3
            pr(5, "feat/gone", GhPrState::Merged, Some(old)),       // not in existing
            pr(6, "main", GhPrState::Merged, Some(old)),            // default → skip
            pr(7, "release/x", GhPrState::Merged, Some(old)),       // protected → skip
            pr(8, "feat/closed", GhPrState::Closed, Some(old)),     // not merged → skip
        ];
        let existing = set(&[
            "feat/merged-old",
            "feat/merged-new",
            "feat/open-reuse",
            "main",
            "release/x",
            "feat/closed",
        ]);
        let protected: HashSet<String> = set(&["main", "release/x"]);

        let got = select_remote_orphans_to_delete(
            &prs,
            &existing,
            &protected,
            "main",
            now,
            REMOTE_ORPHAN_MIN_AGE,
        );
        assert_eq!(
            got,
            vec!["feat/merged-old".to_string()],
            "only the merged, old-enough, still-existing, non-open, non-protected branch is GC'd"
        );
    }

    /// #1750-B4: a branch backing several merged PRs over time is emitted once.
    #[test]
    fn select_dedups_repeated_head_ref_1750_b4() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-05T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old = "2026-06-01T00:00:00Z";
        let prs = vec![
            pr(1, "feat/x", GhPrState::Merged, Some(old)),
            pr(2, "feat/x", GhPrState::Merged, Some(old)),
        ];
        let got = select_remote_orphans_to_delete(
            &prs,
            &set(&["feat/x"]),
            &HashSet::new(),
            "main",
            now,
            REMOTE_ORPHAN_MIN_AGE,
        );
        assert_eq!(got, vec!["feat/x".to_string()]);
    }
}
