//! #817 daemon-side stale local branch cleanup.
//!
//! Operator-triggered hygiene sweep that categorizes local branches
//! into 4 buckets (`clean_merged`, `squash_merged`, `stale_idle`,
//! `active_unknown`) and offers a dry-run + confirm-subset workflow
//! to delete only the proven-safe ones. Mirrors the `tasks::sweep_impl` pattern
//! from #806 — same `dry-run + confirm_ids + system identity +
//! audit_reason` shape — but operates on local git refs instead of
//! the task board.
//!
//! Local Git provides the primary evidence; a configured GitHub remote is
//! queried only for the apply-time open-PR preservation gate. Cache layer
//! (in-memory `HashMap` per sweep) dedups repeated cherry calls when branches
//! share ancestry.
//!
//! Safety stack (mirrors #806 + force-delete-specific layers):
//! - `system:branch_sweep` identity (allow-list at tasks.rs:485)
//! - dry-run default; apply requires explicit `apply=true`
//! - `confirm_ids` MUST be a subset of the current dry-run inventory; only
//!   proven terminal IDs can pass the apply classifier
//! - `audit_reason` required, non-empty
//! - `active_unknown` bucket is always preserved, even when an operator
//!   includes its ID in `confirm_ids`; it remains visible for follow-up
//! - `event_log.jsonl` records `branch=<name> source=<sha>` so an
//!   operator can `git branch <name> <sha>` to restore

use std::path::Path;

#[cfg(test)]
thread_local! {
    static CLEANUP_TEST_PROBE_MASK: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn cleanup_test_probe_mask() -> u8 {
    CLEANUP_TEST_PROBE_MASK.with(std::cell::Cell::get)
}

#[cfg(test)]
fn cleanup_test_probe(mask: u8) {
    CLEANUP_TEST_PROBE_MASK.with(|probes| probes.set(probes.get() | mask));
}

/// PR-A preservation classification is dry-run observability only. None of
/// these values participate in `candidate_ids`, confirmation, or apply.
#[derive(Debug, serde::Serialize)]
struct PreservationEvidence {
    classification: &'static str,
    durable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unique_commit_count: Option<usize>,
    note: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SpikeResidueAnnotation {
    name: String,
    tip_sha: String,
    annotation: &'static str,
}

#[derive(Debug)]
enum ExternalInventory {
    Available(Vec<String>),
    LookupFailed(String),
}

/// Keep a network-backed dry-run probe below the MCP proxy budget. The result
/// is computed once and reused for every reviewer candidate in the scan.
const EXTERNAL_REF_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Threshold for `stale_idle` category. Branches whose tip commit
/// committer-date is older than this AND not merged AND not squash-
/// merged land in `stale_idle`. Operator can override via
/// `min_age_days` arg on the MCP call. Dead-code allow lifts at C3
/// when the MCP handler reads the default.
pub(crate) const STALE_IDLE_DEFAULT_DAYS: i64 = 90;

/// Lightweight enumeration of a local branch — what `git for-each-ref`
/// returns. The category is computed separately via per-branch
/// `git cherry` / `git branch --merged` checks.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct BranchInfo {
    pub name: String,
    pub tip_sha: String,
    /// RFC3339 committer date of the branch tip.
    pub committer_date: String,
}

/// Categorization bucket. Each non-terminal local branch lands in
/// exactly one bucket (first match wins, order: clean_merged →
/// squash_merged → stale_idle → active_unknown).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Candidate {
    pub name: String,
    pub tip_sha: String,
    pub reason: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct Categories {
    pub clean_merged: Vec<Candidate>,
    pub squash_merged: Vec<Candidate>,
    pub stale_idle: Vec<Candidate>,
    pub active_unknown: Vec<Candidate>,
    /// #852 PR-C: reviewer-checkout residue. Naming patterns
    /// `tmp.*` / `pr\d+_head` / `review/.*` that historically
    /// accumulated when reviewer agents `cd canonical && git
    /// checkout <sha>` (the bug PR-A documented and PR-B
    /// enforced at the shim). These branches have no legitimate
    /// purpose and land in the default delete list — but the
    /// daemon boot sweep is dry-run-only for r0 so operator can
    /// validate the regex against their real residue before any
    /// destructive action.
    pub reviewer_checkout: Vec<Candidate>,
}

impl Categories {
    /// Concatenated sorted list of all candidate branch names across
    /// the deletable buckets (clean_merged + squash_merged +
    /// stale_idle + #852 PR-C reviewer_checkout). `active_unknown` is
    /// NOT in this default list — the operator must explicitly pick
    /// those IDs by their bucket.
    pub fn deletable_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .chain(self.reviewer_checkout.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Total IDs including the non-deletable `active_unknown` bucket. The
    /// handler uses this inventory to reject stale IDs while keeping unknown
    /// branches visible; the lifecycle classifier still preserves them.
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .chain(self.reviewer_checkout.iter())
            .chain(self.active_unknown.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn total(&self) -> usize {
        self.all_ids().len()
    }
}

/// Enumerate local branches via `git for-each-ref`, parsing name +
/// tip SHA + ISO-8601 committerdate per line.
fn enumerate_branches(repo: &Path) -> Result<Vec<BranchInfo>, String> {
    // W1.2: git_cmd = always-bypass + bounded + trimmed stdout; its GitError
    // covers both the spawn-fail and non-zero-exit branches this used to handle
    // separately (same semantics, more structured message).
    let stdout = crate::git_helpers::git_cmd(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)|%(objectname)|%(committerdate:iso8601-strict)",
            "refs/heads/",
        ],
    )
    .map_err(|e| format!("git for-each-ref: {e}"))?;
    let branches: Vec<BranchInfo> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let name = parts.next()?.trim().to_string();
            let tip_sha = parts.next()?.trim().to_string();
            let committer_date = parts.next()?.trim().to_string();
            if name.is_empty() || tip_sha.is_empty() {
                return None;
            }
            Some(BranchInfo {
                name,
                tip_sha,
                committer_date,
            })
        })
        .collect();
    Ok(branches)
}

fn checked_is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = crate::git_helpers::git_bypass_timeout(
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        crate::git_helpers::LOCAL_GIT_TIMEOUT,
    )
    .map_err(|e| format!("git merge-base --is-ancestor {ancestor} {descendant}: {e}"))?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(format!(
            "git merge-base --is-ancestor {ancestor} {descendant}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn external_inventory(repo: &Path) -> ExternalInventory {
    let local = match crate::git_helpers::git_cmd(
        repo,
        &["for-each-ref", "--format=%(objectname)", "refs/remotes/"],
    ) {
        Ok(stdout) => stdout,
        Err(e) => {
            return ExternalInventory::LookupFailed(format!(
                "local remote-tracking ref enumeration failed: {e}"
            ));
        }
    };

    let remote = match crate::git_helpers::git_bypass_timeout(
        repo,
        &[
            "ls-remote",
            "--refs",
            "origin",
            "refs/heads/*",
            "refs/pull/*/head",
        ],
        EXTERNAL_REF_PROBE_TIMEOUT,
    ) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        Ok(output) => {
            return ExternalInventory::LookupFailed(format!(
                "origin refs/pull lookup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Err(e) => {
            return ExternalInventory::LookupFailed(format!(
                "origin refs/pull lookup failed or timed out after {}s: {e}",
                EXTERNAL_REF_PROBE_TIMEOUT.as_secs()
            ));
        }
    };

    let mut roots: Vec<String> = local
        .lines()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(String::from)
        .chain(remote.lines().filter_map(|line| {
            line.split_whitespace()
                .next()
                .filter(|sha| !sha.is_empty())
                .map(String::from)
        }))
        .collect();
    roots.sort();
    roots.dedup();
    ExternalInventory::Available(roots)
}

fn rev_list_count_excluding(
    repo: &Path,
    tip: &str,
    exclusions: &[String],
) -> Result<std::process::Output, String> {
    let mut args = vec![
        "rev-list".to_string(),
        "--count".to_string(),
        tip.to_string(),
        "--not".to_string(),
    ];
    args.extend(exclusions.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::git_helpers::git_bypass_timeout(repo, &arg_refs, crate::git_helpers::LOCAL_GIT_TIMEOUT)
        .map_err(|e| format!("git rev-list --count {tip}: {e}"))
}

fn parse_rev_list_count(output: &std::process::Output, context: &str) -> Result<usize, String> {
    if !output.status.success() {
        return Err(format!(
            "git rev-list {context}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .map_err(|e| format!("invalid rev-list count for {context}: {e}"))
}

fn unique_commit_count(
    repo: &Path,
    candidate: &BranchInfo,
    base: &str,
    branches: &[BranchInfo],
    external_roots: &[String],
) -> Result<usize, String> {
    let mut exclusions: Vec<String> = vec![base.to_string()];
    exclusions.extend(
        branches
            .iter()
            .filter(|branch| branch.name != candidate.name)
            .map(|branch| branch.tip_sha.clone()),
    );
    exclusions.extend(external_roots.iter().cloned());
    exclusions.sort();
    exclusions.dedup();

    let output = rev_list_count_excluding(repo, &candidate.tip_sha, &exclusions)?;
    parse_rev_list_count(&output, &format!("unique count for {}", candidate.name))
}

fn classify_preservation(
    repo: &Path,
    base: &str,
    candidate: &BranchInfo,
    branches: &[BranchInfo],
    external: &ExternalInventory,
) -> Result<PreservationEvidence, String> {
    if checked_is_ancestor(repo, &candidate.tip_sha, base)? {
        return Ok(PreservationEvidence {
            classification: "MAIN_REACHABLE",
            durable: false,
            unique_commit_count: None,
            note: format!(
                "tip is currently reachable from {base}; current reachability is not durable preservation"
            ),
        });
    }

    let roots = match external {
        ExternalInventory::LookupFailed(error) => {
            return Ok(PreservationEvidence {
                classification: "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
                durable: false,
                unique_commit_count: None,
                note: error.clone(),
            });
        }
        ExternalInventory::Available(roots) => roots,
    };

    if roots.iter().any(|root| root == &candidate.tip_sha) {
        return Ok(PreservationEvidence {
            classification: "EXTERNALLY_REACHABLE_UNGUARANTEED",
            durable: false,
            unique_commit_count: None,
            note: "candidate tip exactly matches a current external ref; external reachability is not durable preservation".to_string(),
        });
    }

    if !roots.is_empty() {
        // One graph walk answers whether the candidate tip is reachable from
        // ANY external root. This keeps local work O(candidates), not
        // O(candidates × refs), after the single cached remote probe.
        let output = rev_list_count_excluding(repo, &candidate.tip_sha, roots)?;
        if !output.status.success() {
            return Ok(PreservationEvidence {
                classification: "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
                durable: false,
                unique_commit_count: None,
                note: format!(
                    "external ref ancestry could not be proven from local objects: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        if parse_rev_list_count(&output, "external reachability")? == 0 {
            return Ok(PreservationEvidence {
                classification: "EXTERNALLY_REACHABLE_UNGUARANTEED",
                durable: false,
                unique_commit_count: None,
                note: "candidate tip is currently reachable from an external ref; external reachability is not durable preservation".to_string(),
            });
        }
    }

    let count = unique_commit_count(repo, candidate, base, branches, roots)?;
    Ok(PreservationEvidence {
        classification: "ORPHANED_UNIQUE",
        durable: false,
        unique_commit_count: Some(count),
        note: "external inventory succeeded; count is current unique reachability, not deletion authorization"
            .to_string(),
    })
}

pub(crate) fn dry_run_observability(
    repo: &Path,
    base: &str,
    categories: &Categories,
) -> Result<(serde_json::Value, Vec<SpikeResidueAnnotation>), String> {
    let branches = enumerate_branches(repo)?;
    let by_name: std::collections::HashMap<&str, &BranchInfo> = branches
        .iter()
        .map(|branch| (branch.name.as_str(), branch))
        .collect();
    let spike_residue = branches
        .iter()
        .filter(|branch| branch.name.starts_with("spike/"))
        .map(|branch| SpikeResidueAnnotation {
            name: branch.name.clone(),
            tip_sha: branch.tip_sha.clone(),
            annotation: "SPIKE_RESIDUE",
        })
        .collect();

    let mut needs_external = false;
    for candidate in &categories.reviewer_checkout {
        // A reviewer branch may have been deleted between scan() and this
        // observability call (concurrent cleanup, worktree release, manual
        // deletion). Skip absent branches instead of aborting the dry-run.
        let Some(branch) = by_name.get(candidate.name.as_str()) else {
            continue;
        };
        if !checked_is_ancestor(repo, &branch.tip_sha, base)? {
            needs_external = true;
            break;
        }
    }
    let external = if needs_external {
        external_inventory(repo)
    } else {
        ExternalInventory::Available(Vec::new())
    };

    let mut serialized = serde_json::to_value(categories)
        .map_err(|e| format!("serialize branch sweep categories: {e}"))?;
    let reviewer_candidates = serialized["reviewer_checkout"]
        .as_array_mut()
        .ok_or_else(|| "serialized reviewer_checkout was not an array".to_string())?;
    for candidate in reviewer_candidates {
        let name = candidate["name"]
            .as_str()
            .ok_or_else(|| "serialized reviewer candidate missing name".to_string())?;
        // Same as above: skip absent reviewer branches instead of aborting.
        let Some(branch) = by_name.get(name) else {
            continue;
        };
        let evidence = classify_preservation(repo, base, branch, &branches, &external)?;
        candidate["preservation"] = serde_json::to_value(evidence)
            .map_err(|e| format!("serialize preservation evidence for {name}: {e}"))?;
    }
    Ok((serialized, spike_residue))
}

/// Returns true if `branch` is reachable from `base` via a merge
/// commit (`git branch --merged base` includes it). Used to detect
/// the `clean_merged` category.
fn is_clean_merged(repo: &Path, base: &str, branch: &str) -> bool {
    // W1.2: git_cmd → trimmed stdout on success; both the spawn-error and
    // non-zero-exit `return false` branches collapse to the `Err → false` arm.
    let Ok(stdout) = crate::git_helpers::git_cmd(repo, &["branch", "--merged", base]) else {
        return false;
    };
    stdout
        .lines()
        .map(|line| {
            line.trim_start_matches(|ch| {
                // `git branch` prefixes the current checkout with `*`; the
                // fleet-managed git shim additionally uses `+` for a branch held by
                // another worktree. Both prefixes still identify the branch name.
                ch == '*' || ch == '+'
            })
            .trim()
        })
        .any(|line| line == branch)
}

/// Returns true if every commit on `branch` is already applied to
/// `base` as an equivalent patch (squash-merged). `git cherry base
/// branch` output prefix per commit: `-` means present in base, `+`
/// means missing. All-`-` (and at least one line) ⇒ squash-merged.
///
/// #1280: Falls back to tree-diff comparison when `git cherry` misses
/// GitHub-style squash merges (single squashed commit has a different
/// patch-id than the individual commits). The fallback checks if the
/// diff from merge-base to the branch tip is empty against base HEAD
/// (i.e., all changes are already incorporated).
// #1750-B3: pub(crate) so the automatic per-tick GC
// (`worktree_cleanup::prune_orphaned_branches`) reuses the SAME squash-merge
// detection the operator-triggered sweep uses — the squash-blind `git branch
// --merged` in the auto path missed 95/99 squash-orphan branches.
pub(crate) fn is_squash_merged(repo: &Path, base: &str, branch: &str) -> bool {
    // Method 1: git cherry (works for cherry-picked commits).
    if is_squash_merged_cherry(repo, base, branch) {
        return true;
    }
    // Method 2: tree-diff comparison (works for GitHub squash-merge).
    is_squash_merged_diff(repo, base, branch)
}

/// `git cherry` based detection.
fn is_squash_merged_cherry(repo: &Path, base: &str, branch: &str) -> bool {
    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero-exit
    // both collapse to the `Err → false` arm.
    let Ok(stdout) = crate::git_helpers::git_cmd(repo, &["cherry", base, branch]) else {
        return false;
    };
    let mut had_any = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        had_any = true;
        if !trimmed.starts_with('-') {
            return false;
        }
    }
    had_any
}

/// Tri-state result of the PR-based (authoritative) merge check. `Unknown`
/// means the check could NOT run — no github remote, `extract_github_repo`
/// returned `None`, the tip couldn't be resolved, or the `gh`/scm call errored
/// — as distinct from `NotMerged` (the check ran and found no matching merged
/// PR). #P3 (branch-residue): callers that treat a merged PR as monotonic proof
/// (delete NOW, no age gate) act ONLY on `Merged`; `Unknown` fails CLOSED
/// (treated as not-merged) everywhere, so a gh outage never reaps a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrMergeStatus {
    Merged,
    NotMerged,
    Unknown,
}

/// Tri-state open-PR probe used by branch lifecycle retirement. A repository
/// without a GitHub remote has no open-PR surface to query (`NotOpen`), while
/// a GitHub/SCM lookup failure is `Unknown` and must fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenPrStatus {
    Open,
    NotOpen,
    Unknown,
}

/// One bounded open-PR inventory for a repository/sweep.  The daemon cleanup
/// path must not issue one SCM lookup per branch; a failed inventory remains
/// `Unknown` for every affected branch so the lifecycle classifier preserves
/// them all.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenPrSnapshot {
    open_branches: Option<std::collections::HashSet<String>>,
}

const OPEN_PR_SNAPSHOT_CAP: usize = 1000;

impl OpenPrSnapshot {
    pub(crate) fn status_for(&self, branch: &str) -> OpenPrStatus {
        match &self.open_branches {
            Some(open) if open.contains(branch) => OpenPrStatus::Open,
            Some(_) => OpenPrStatus::NotOpen,
            None => OpenPrStatus::Unknown,
        }
    }
}

/// Gather the repository's open-PR inventory once.  `headRefName` is the only
/// field needed by the lifecycle gate; the explicit limit keeps this external
/// call bounded even for a repository with a large review backlog.
pub(crate) fn open_pr_snapshot(repo: &Path, _base: &str) -> OpenPrSnapshot {
    let remote_url = match crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) {
        Ok(url) => url,
        Err(crate::git_helpers::GitError::NonZero { stderr, .. })
            if stderr.contains("No such remote") =>
        {
            return OpenPrSnapshot {
                open_branches: Some(std::collections::HashSet::new()),
            };
        }
        Err(_) => return OpenPrSnapshot::default(),
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        return OpenPrSnapshot::default();
    };
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("open"),
            // Open PRs targeting any base protect the branch. Request one
            // beyond the bounded inventory so truncation is distinguishable
            // from a complete result and remains fail-closed.
            base: None,
            limit: Some((OPEN_PR_SNAPSHOT_CAP + 1) as u32),
            ..Default::default()
        },
        &["headRefName"],
        None,
    ) else {
        return OpenPrSnapshot::default();
    };
    if prs.len() > OPEN_PR_SNAPSHOT_CAP {
        return OpenPrSnapshot::default();
    }
    let mut open_branches = std::collections::HashSet::new();
    for pr in prs {
        let Some(branch) = pr.head_ref else {
            // A malformed/partial provider response is not proof that the
            // branch has no open PR; preserve all candidates on this snapshot.
            return OpenPrSnapshot::default();
        };
        open_branches.insert(branch);
    }
    OpenPrSnapshot {
        open_branches: Some(open_branches),
    }
}

/// Resolve whether `branch` still has an open PR. The lifecycle classifier
/// owns the fail direction; this helper only gathers the SCM evidence.
pub(crate) fn open_pr_status(repo: &Path, _base: &str, branch: &str) -> OpenPrStatus {
    let remote_url = match crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) {
        Ok(url) => url,
        Err(crate::git_helpers::GitError::NonZero { stderr, .. })
            if stderr.contains("No such remote") =>
        {
            // No remote is a deterministic local-fixture/no-PR state, not a
            // transient SCM outage.
            return OpenPrStatus::NotOpen;
        }
        Err(_) => return OpenPrStatus::Unknown,
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        // A configured non-GitHub remote is an unresolved SCM surface, not
        // evidence that the branch has no open review. Keep the lifecycle
        // decision fail-closed until a provider can answer it.
        return OpenPrStatus::Unknown;
    };
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("open"),
            head: Some(branch.to_string()),
            // A branch remains protected by an open PR regardless of target
            // base; do not narrow this apply-time probe to the repository's
            // default branch.
            base: None,
            ..Default::default()
        },
        &["number"],
        None,
    ) else {
        return OpenPrStatus::Unknown;
    };
    if prs.is_empty() {
        OpenPrStatus::NotOpen
    } else {
        OpenPrStatus::Open
    }
}

/// GitHub API based detection: query whether a merged PR exists for this
/// branch with matching HEAD SHA. Most reliable — not affected by git history
/// topology. SHA check prevents false positives from branch name reuse.
///
/// #P3: returns a TRI-STATE (`PrMergeStatus`) so a caller can tell "detection
/// couldn't run" (`Unknown`) apart from "ran, no matching merged PR"
/// (`NotMerged`). The private `is_squash_merged_diff` wrapper below collapses
/// `Merged → true` / else → false to keep `is_squash_merged`'s Method-2
/// behavior byte-identical.
pub(crate) fn pr_merge_status(repo: &Path, base: &str, branch: &str) -> PrMergeStatus {
    // Resolve owner/repo from git remote origin.
    // W1.2 class-2: git_cmd always adds AGEND_GIT_BYPASS + trims stdout (this
    // site previously ran raw `git` — the forgot-bypass latent class #821/#1463).
    let Ok(remote_url) = crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]) else {
        return PrMergeStatus::Unknown;
    };
    let Some(gh_repo) = extract_github_repo(&remote_url) else {
        return PrMergeStatus::Unknown;
    };
    // Get local branch tip SHA.
    let Ok(local_sha) = crate::git_helpers::git_cmd(repo, &["rev-parse", branch]) else {
        return PrMergeStatus::Unknown;
    };
    // #PR-D: `gh pr list` via ScmProvider. argv is set-equal to the prior
    // inline `pr list --state merged --head B --base BASE --repo R --json
    // headRefOid` — flag ORDER is canonicalized (gh order-insensitive) per
    // decision d-20260601151209762922-0; same flags+values. Uses --repo
    // (gh_repo derived above), no cwd. A gh/scm error → `Unknown` (fail-closed).
    let Ok(prs) = crate::scm::make_scm_provider(&gh_repo, None).pr_list(
        &gh_repo,
        &crate::scm::ListFilter {
            state: Some("merged"),
            head: Some(branch.to_string()),
            base: Some(base.to_string()),
            ..Default::default()
        },
        &["headRefOid"],
        None,
    ) else {
        return PrMergeStatus::Unknown;
    };
    // Merged iff any merged PR's HEAD SHA matches the local branch tip, or the
    // local tip is a strict ancestor of that HEAD SHA — see
    // `local_sha_matches_merged_head` for why the ancestor case matters.
    let merged = prs.iter().any(|s| {
        s.head_ref_oid
            .as_deref()
            .is_some_and(|oid| local_sha_matches_merged_head(repo, &local_sha, oid))
    });
    if merged {
        PrMergeStatus::Merged
    } else {
        PrMergeStatus::NotMerged
    }
}

/// Method-2 wrapper for [`is_squash_merged`]: `Merged → true`, else false.
/// `Unknown` maps to NOT squash-merged — byte-identical to the pre-#P3
/// `is_squash_merged_diff` (every non-`Merged` outcome was already `false`).
fn is_squash_merged_diff(repo: &Path, base: &str, branch: &str) -> bool {
    matches!(pr_merge_status(repo, base, branch), PrMergeStatus::Merged)
}

/// True iff `head_ref_oid` (a merged PR's recorded HEAD SHA) equals
/// `local_sha`, or `local_sha` is a strict ancestor of it.
///
/// t-20260704054810920172-67777-3: main's now-default strict-up-to-date
/// branch protection means a required "Update branch" sync commit lands on
/// the remote HEAD before the squash-merge — but this sweep's
/// `fetch --prune` only refreshes remote-tracking refs, never fast-forwards
/// the local branch ref itself, so `local_sha` stays one sync-commit behind
/// `head_ref_oid` forever once the remote branch is deleted. is-ancestor
/// accepts "local's own work is a strict prefix of what was actually merged"
/// as proof; the caller's `state: "merged"` filter already guarantees
/// `head_ref_oid` came from an actually-merged PR, so no unmerged work can
/// ever satisfy this check (reflexive when equal, so this strictly extends
/// rather than replaces the old exact-match behavior). Fails CLOSED (not a
/// match) if the ancestor check itself errors — e.g. `head_ref_oid`'s commit
/// no longer exists locally after the remote branch's deletion — via
/// `git_ok`'s exit-code-0-only success semantics.
fn local_sha_matches_merged_head(repo: &Path, local_sha: &str, head_ref_oid: &str) -> bool {
    head_ref_oid == local_sha
        || crate::git_helpers::git_ok(
            repo,
            &["merge-base", "--is-ancestor", local_sha, head_ref_oid],
        )
}

pub(crate) fn extract_github_repo_for_intent(url: &str) -> Option<String> {
    extract_github_repo(url)
}

/// Return the PR number of a merged PR whose head matches the local branch tip.
/// Used by cleanup intent sweep to independently verify PR generation.
pub(crate) fn merged_pr_number(repo: &Path, base: &str, branch: &str) -> Option<u64> {
    let remote_url = crate::git_helpers::git_cmd(repo, &["remote", "get-url", "origin"]).ok()?;
    let gh_repo = extract_github_repo(&remote_url)?;
    let local_sha = crate::git_helpers::git_cmd(repo, &["rev-parse", branch]).ok()?;
    let prs = crate::scm::make_scm_provider(&gh_repo, None)
        .pr_list(
            &gh_repo,
            &crate::scm::ListFilter {
                state: Some("merged"),
                head: Some(branch.to_string()),
                base: Some(base.to_string()),
                ..Default::default()
            },
            &["headRefOid", "number"],
            None,
        )
        .ok()?;
    prs.iter()
        .find(|pr| {
            pr.head_ref_oid
                .as_deref()
                .is_some_and(|oid| local_sha_matches_merged_head(repo, &local_sha, oid))
        })
        .map(|pr| pr.number)
}

/// Extract "owner/repo" from a GitHub remote URL.
fn extract_github_repo(url: &str) -> Option<String> {
    // Handles: https://github.com/owner/repo.git, git@github.com:owner/repo.git
    let stripped = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if stripped.contains("github.com") {
        if let Some(path) = stripped.strip_prefix("git@github.com:") {
            return Some(path.to_string());
        }
        // https://github.com/owner/repo
        if let Some(idx) = stripped.find("github.com/") {
            return Some(stripped[idx + "github.com/".len()..].to_string());
        }
    }
    None
}

/// #817 scan local branches and categorize into the 4 buckets.
/// `now` parameterized so `stale_idle` threshold testing isn't
/// flaky around day boundaries. Dead-code allow lifts at C3 when
/// the MCP handler wires the call site.
/// #852 PR-C: classify reviewer-checkout residue by name. Pattern
/// covers the three observed pollution shapes:
/// - `tmp.*` — operator's `tmp_pr_review` / `tmp/abc1234` style
/// - `pr\d+_head` — `gh pr fetch`-style `pr123_head` refs
/// - `review/.*` — explicit `review/<n>` namespace
///
/// First-match wins. Conservative — empty / `main` / `master` /
/// genuine branch prefixes never match. Uses an inline anchored
/// regex (`^` anchor explicit, full-string `is_match` semantics on
/// the regex crate) so prefix-match-only is the contract.
pub(crate) fn is_reviewer_checkout(name: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // SAFETY: regex literal is compile-time-validated by the test
    // suite (the pattern's anchor + alternations are exercised by
    // the four `reviewer_checkout_pattern_*` unit tests). `.unwrap`
    // here is the established crate convention for build-time
    // patterns (see `state.rs::StatePatterns::for_backend`).
    #[allow(clippy::unwrap_used)]
    let re = RE.get_or_init(|| regex::Regex::new(r"^(tmp.*|pr\d+_head|review/.*)$").unwrap());
    re.is_match(name)
}

pub(crate) fn scan(
    repo: &Path,
    base: &str,
    min_age_days: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Categories, String> {
    let branches = enumerate_branches(repo)?;
    let mut cats = Categories::default();
    for b in &branches {
        if b.name == base {
            continue;
        }
        // 0. reviewer_checkout (#852 PR-C) — naming-pattern residue.
        // Checked FIRST so reviewer-pollution branches that happen to
        // also satisfy clean_merged / squash_merged conditions still
        // surface in the dedicated bucket (operator can audit them
        // separately from the regular merge-based categories).
        if is_reviewer_checkout(&b.name) {
            cats.reviewer_checkout.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: "reviewer-checkout residue (tmp.* / pr*_head / review/*)".to_string(),
            });
            continue;
        }
        // 1. clean_merged — reachable from base via merge commit.
        if is_clean_merged(repo, base, &b.name) {
            cats.clean_merged.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: format!("merged into {base}"),
            });
            continue;
        }
        // 2. squash_merged — all commits already in base by patch-id.
        if is_squash_merged(repo, base, &b.name) {
            cats.squash_merged.push(Candidate {
                name: b.name.clone(),
                tip_sha: b.tip_sha.clone(),
                reason: format!("all commits squash-applied to {base}"),
            });
            continue;
        }
        // 3. stale_idle — committer date older than threshold.
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&b.committer_date) {
            let age = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
            if age > chrono::Duration::days(min_age_days) {
                cats.stale_idle.push(Candidate {
                    name: b.name.clone(),
                    tip_sha: b.tip_sha.clone(),
                    reason: format!("idle {}d (>{min_age_days}d threshold)", age.num_days()),
                });
                continue;
            }
        }
        // 4. active_unknown — residual.
        cats.active_unknown.push(Candidate {
            name: b.name.clone(),
            tip_sha: b.tip_sha.clone(),
            reason: "unmerged + not squash-applied + within freshness window".to_string(),
        });
    }
    Ok(cats)
}

/// Apply phase — `git branch -D <name>` for each confirm_id under
/// the `system:branch_sweep` identity. Each deletion records a
/// `branch_sweep_apply` entry to `event-log.jsonl` with the source
/// SHA so an operator can `git branch <name> <sha>` to restore.
///
/// Returns the count of successfully deleted branches. A per-branch
/// failure logs the error but does not abort the batch — partial
/// success is observable in the event log.
///
/// Dead-code allow lifts at C3 when the MCP handler wires the call.
#[allow(dead_code)]
pub(crate) fn emit_delete_batch(
    home: &Path,
    repo: &Path,
    categories: &Categories,
    confirm_ids: &std::collections::HashSet<String>,
    audit_reason: &str,
) -> Result<usize, String> {
    emit_delete_batch_with_context(
        Some(home),
        repo,
        "main",
        categories,
        confirm_ids,
        audit_reason,
    )
    .map(|(count, _)| count)
}

/// Apply a branch-sweep confirmation with lifecycle evidence. The legacy
/// wrapper above keeps the existing unit-test seam; production MCP callers
/// pass the explicit base and home so active holders, tasks, and PR state are
/// checked before any branch mutation.
pub(crate) fn emit_delete_batch_with_context(
    home: Option<&Path>,
    repo: &Path,
    base: &str,
    categories: &Categories,
    confirm_ids: &std::collections::HashSet<String>,
    audit_reason: &str,
) -> Result<(usize, Vec<serde_json::Value>), String> {
    // #2011: prune orphaned worktree REGISTRATIONS first, in the same
    // transaction as the branch deletions. A worktree whose physical
    // directory is gone (crashed release, manual rm, pre-prune-era leak)
    // keeps its branch "checked out" in git's eyes → `branch -D` refuses →
    // branches pile up forever (live: 14 stale branches behind 9 prunable
    // registrations, 2026-06-11). Prune is idempotent and cheap; doing it
    // HERE — rather than only at each deletion site — closes the gap
    // regardless of which path leaked the registration (chokepoint
    // principle). Best-effort: a prune failure just leaves the per-branch
    // refusal behavior unchanged (logged below as before).
    if let Err(e) = crate::git_helpers::git_bypass(repo, &["worktree", "prune"]) {
        tracing::warn!(error = %e, "#2011: git worktree prune before branch sweep failed (non-fatal)");
    }
    let mut name_to_candidate: std::collections::HashMap<&str, &Candidate> =
        std::collections::HashMap::new();
    for cand in categories
        .clean_merged
        .iter()
        .chain(categories.squash_merged.iter())
        .chain(categories.stale_idle.iter())
        .chain(categories.reviewer_checkout.iter())
        .chain(categories.active_unknown.iter())
    {
        name_to_candidate.insert(cand.name.as_str(), cand);
    }
    let mut open_pr_inventory: Option<OpenPrSnapshot> = None;
    let category_of = |name: &str| -> &'static str {
        if categories.clean_merged.iter().any(|c| c.name == name) {
            "clean_merged"
        } else if categories.squash_merged.iter().any(|c| c.name == name) {
            "squash_merged"
        } else if categories.stale_idle.iter().any(|c| c.name == name) {
            "stale_idle"
        } else if categories.reviewer_checkout.iter().any(|c| c.name == name) {
            "reviewer_checkout"
        } else {
            "active_unknown"
        }
    };
    let mut deleted = 0usize;
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    for name in confirm_ids {
        let Some(cand) = name_to_candidate.get(name.as_str()) else {
            continue;
        };
        let is_reviewer = categories.reviewer_checkout.iter().any(|c| c.name == *name);
        let is_stale_idle = categories.stale_idle.iter().any(|c| c.name == *name);
        let provenance = if is_reviewer {
            crate::worktree::disposition::BranchProvenance::ReviewerResidue
        } else if categories.clean_merged.iter().any(|c| c.name == *name) {
            crate::worktree::disposition::BranchProvenance::Merged
        } else if categories.squash_merged.iter().any(|c| c.name == *name) {
            crate::worktree::disposition::BranchProvenance::SquashMerged
        } else if is_stale_idle {
            crate::worktree::disposition::BranchProvenance::StaleIdle
        } else {
            // `active_unknown` has no terminal provenance and therefore
            // remains fail-closed in the shared classifier.
            crate::worktree::disposition::BranchProvenance::Unknown
        };
        let terminal = !matches!(
            provenance,
            crate::worktree::disposition::BranchProvenance::Unknown
        );
        let (active_holder, task_active, open_pr) = if !terminal {
            // Unknown provenance is already a KEEP decision; do not probe
            // binding, holder, task, or external SCM state for it.
            (None, None, Some(false))
        } else {
            #[cfg(test)]
            cleanup_test_probe(0b001);
            let binding_active = home
                .and_then(|h| crate::worktree_cleanup::branch_has_active_binding(h, repo, name));
            #[cfg(test)]
            cleanup_test_probe(0b010);
            let active_holder = match (branch_is_checked_out(repo, name), binding_active) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
            #[cfg(test)]
            cleanup_test_probe(0b100);
            let task_active = home.and_then(|h| branch_has_active_task(h, name));
            let inventory = open_pr_inventory.get_or_insert_with(|| open_pr_snapshot(repo, base));
            let open_pr = match inventory.status_for(name) {
                OpenPrStatus::Open => Some(true),
                OpenPrStatus::NotOpen => Some(false),
                OpenPrStatus::Unknown => None,
            };
            (active_holder, task_active, open_pr)
        };
        // Reviewer residue is only deleted after a recovery ref is prepared
        // below; all other proven terminal categories are already preserved
        // by their merge/squash provenance.
        let unique_unpreserved_work = Some(false);
        let lifecycle = crate::worktree::disposition::branch_lifecycle_disposition(
            &crate::worktree::disposition::BranchLifecycleInput {
                provenance,
                terminal,
                active_holder,
                task_active,
                open_pr,
                unique_unpreserved_work,
            },
        );
        if !matches!(
            lifecycle,
            crate::worktree::disposition::BranchLifecycleDisposition::Delete
        ) {
            let blocker = first_lifecycle_blocker(
                terminal,
                active_holder,
                task_active,
                open_pr,
                unique_unpreserved_work,
                provenance,
            );
            crate::event_log::log(
                home.unwrap_or(repo),
                "branch_sweep_apply_skipped",
                "system:branch_sweep",
                &format!("branch={name} blocker={blocker}"),
            );
            skipped.push(serde_json::json!({"branch": name, "blocker": blocker}));
            continue;
        }
        let recovery_ref = if is_reviewer || is_stale_idle {
            Some(prepare_branch_recovery(
                home,
                repo,
                name,
                &cand.tip_sha,
                audit_reason,
            )?)
        } else {
            None
        };
        let _ = recovery_ref;
        // W1.2: git_cmd's GitError preserves the two distinct failure logs this
        // site emits — NonZero carries the trimmed stderr, Spawn carries the io error.
        match crate::git_helpers::git_cmd(repo, &["branch", "-D", name]) {
            Ok(_) => {
                deleted += 1;
                let category = category_of(name);
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply",
                    "system:branch_sweep",
                    &format!(
                        "branch={name} category={category} sha={tip} reason={audit_reason} \
                         restore_hint=`git branch {name} {tip}`",
                        tip = cand.tip_sha
                    ),
                );
            }
            Err(crate::git_helpers::GitError::NonZero { stderr, .. }) => {
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} stderr={stderr}"),
                );
            }
            Err(crate::git_helpers::GitError::Spawn(e)) => {
                crate::event_log::log(
                    home.unwrap_or(repo),
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} spawn_error={e}"),
                );
            }
        }
    }
    Ok((deleted, skipped))
}

fn first_lifecycle_blocker(
    terminal: bool,
    active_holder: Option<bool>,
    task_active: Option<bool>,
    open_pr: Option<bool>,
    unique_unpreserved_work: Option<bool>,
    provenance: crate::worktree::disposition::BranchProvenance,
) -> &'static str {
    if !terminal {
        return "non_terminal";
    }
    if active_holder != Some(false) {
        return if active_holder == Some(true) {
            "active_holder"
        } else {
            "active_holder_unknown"
        };
    }
    if task_active != Some(false) {
        return if task_active == Some(true) {
            "task_active"
        } else {
            "task_active_unknown"
        };
    }
    if open_pr != Some(false) {
        return if open_pr == Some(true) {
            "open_pr"
        } else {
            "open_pr_status_unknown"
        };
    }
    if unique_unpreserved_work != Some(false) {
        return if unique_unpreserved_work == Some(true) {
            "unique_unpreserved_work"
        } else {
            "unique_unpreserved_work_unknown"
        };
    }
    if matches!(
        provenance,
        crate::worktree::disposition::BranchProvenance::Unknown
    ) {
        return "provenance_unknown";
    }
    "unknown"
}

fn branch_is_checked_out(repo: &Path, branch: &str) -> Option<bool> {
    let out = crate::git_helpers::git_cmd(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut live_worktree = false;
    let mut prunable = false;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            live_worktree = Path::new(path.trim()).exists();
            prunable = false;
            continue;
        }
        if line.starts_with("prunable ") {
            prunable = true;
            live_worktree = false;
            continue;
        }
        if line
            .strip_prefix("branch refs/heads/")
            .is_some_and(|name| name.trim() == branch)
            && live_worktree
            && !prunable
        {
            return Some(true);
        }
    }
    Some(false)
}

pub(crate) fn branch_has_active_task(home: &Path, branch: &str) -> Option<bool> {
    note_active_task_probe();
    let tasks = crate::tasks::list_all_strict(home).ok()?;
    Some(tasks.iter().any(|task| {
        task.branch.as_deref() == Some(branch)
            && !matches!(
                task.status,
                crate::task_events::TaskStatus::Done
                    | crate::task_events::TaskStatus::Cancelled
                    | crate::task_events::TaskStatus::Verified
            )
    }))
}

/// Create a durable recovery ref for a reviewer residue before deleting its
/// branch. The source SHA is the CAS identity; the returned ref is the
/// operator-visible recovery/audit identity.
pub(crate) fn prepare_branch_recovery(
    home: Option<&Path>,
    repo: &Path,
    branch: &str,
    tip_sha: &str,
    reason: &str,
) -> Result<String, String> {
    if tip_sha.is_empty() {
        return Err(format!("branch '{branch}' has no source SHA; preserved"));
    }
    let safe_branch: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let identity = format!(
        "refs/agend/recovery/branch/{safe_branch}/{}-{}",
        &tip_sha[..tip_sha.len().min(12)],
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let out = crate::git_helpers::git_bypass(repo, &["update-ref", &identity, tip_sha])
        .map_err(|e| format!("prepare recovery ref for '{branch}' failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "prepare recovery ref for '{branch}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if let Some(home) = home {
        crate::event_log::log(
            home,
            "branch_cleanup_prepared",
            "system:branch_lifecycle",
            &format!(
                "repo={} branch={branch} source_sha={tip_sha} recovery_ref={identity} reason={reason}",
                repo.display()
            ),
        );
    }
    Ok(identity)
}

// #2999: counts calls into `branch_has_active_task`, to prove that
// non-terminal candidates in `prune_orphaned_branches_with_home` skip the
// strict task-ledger replay.
#[cfg(test)]
std::thread_local! {
    static ACTIVE_TASK_PROBE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_active_task_probe() {
    ACTIVE_TASK_PROBE_COUNT.with(|count| count.set(count.get() + 1));
}
#[cfg(not(test))]
fn note_active_task_probe() {}

/// Read the count and zero it — so a caller can also use this as the "start
/// from zero" setup step, with no separate reset accessor.
#[cfg(test)]
pub(crate) fn take_active_task_probe_count() -> usize {
    ACTIVE_TASK_PROBE_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
#[path = "branch_sweep/tests.rs"]
mod tests;
