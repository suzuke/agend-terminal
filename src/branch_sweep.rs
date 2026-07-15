//! #817 daemon-side stale local branch cleanup.
//!
//! Operator-triggered hygiene sweep that categorizes local branches
//! into 4 buckets (`clean_merged`, `squash_merged`, `stale_idle`,
//! `active_unknown`) and offers a dry-run + confirm-subset workflow
//! to delete the safe ones. Mirrors the `tasks::sweep_impl` pattern
//! from #806 — same `dry-run + confirm_ids + system identity +
//! audit_reason` shape — but operates on local git refs instead of
//! the task board.
//!
//! No GitHub API dependency. Everything is local `git for-each-ref` /
//! `git cherry` / `git branch -D` subprocess. Cache layer (in-memory
//! `HashMap` per sweep) dedups repeated cherry calls when branches
//! share ancestry.
//!
//! Safety stack (mirrors #806 + force-delete-specific layers):
//! - `system:branch_sweep` identity (allow-list at tasks.rs:485)
//! - dry-run default; apply requires explicit `apply=true`
//! - `confirm_ids` MUST be subset of `candidate_ids` from prior dry-run
//! - `audit_reason` required, non-empty
//! - `active_unknown` bucket skipped unless operator explicitly picks
//!   those IDs (the bucket itself surfaces in dry-run for visibility)
//! - `event_log.jsonl` records `branch=<name> source=<sha>` so an
//!   operator can `git branch <name> <sha>` to restore

use std::path::Path;

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

    /// Total IDs including the explicit-opt-in `active_unknown`
    /// bucket. Used to validate confirm_ids subset — operator CAN
    /// pick active_unknown IDs, they just don't show up in the
    /// default `deletable_ids`.
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
        let branch = by_name.get(candidate.name.as_str()).ok_or_else(|| {
            format!(
                "review candidate {} missing from branch inventory",
                candidate.name
            )
        })?;
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
        let branch = by_name
            .get(name)
            .ok_or_else(|| format!("review candidate {name} missing from branch inventory"))?;
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
        .map(|l| l.trim_start_matches('*').trim())
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
pub(crate) fn emit_delete_batch(
    home: &Path,
    repo: &Path,
    categories: &Categories,
    confirm_ids: &std::collections::HashSet<String>,
    audit_reason: &str,
) -> Result<usize, String> {
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
    for name in confirm_ids {
        let Some(cand) = name_to_candidate.get(name.as_str()) else {
            continue;
        };
        // W1.2: git_cmd's GitError preserves the two distinct failure logs this
        // site emits — NonZero carries the trimmed stderr, Spawn carries the io error.
        match crate::git_helpers::git_cmd(repo, &["branch", "-D", name]) {
            Ok(_) => {
                deleted += 1;
                let category = category_of(name);
                crate::event_log::log(
                    home,
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
                    home,
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} stderr={stderr}"),
                );
            }
            Err(crate::git_helpers::GitError::Spawn(e)) => {
                crate::event_log::log(
                    home,
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!("branch={name} spawn_error={e}"),
                );
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── #852 PR-C — reviewer_checkout pattern unit tests ──────────────

    /// `tmp_pr_review` / `tmp/abc1234` / `tmp-merge` — the operator-
    /// created scratch branches that show up after `cd canonical &&
    /// git checkout -b tmp_<...>`. Must classify as reviewer_checkout.
    #[test]
    fn reviewer_checkout_pattern_matches_tmp_prefix() {
        assert!(
            is_reviewer_checkout("tmp_pr_review"),
            "`tmp_pr_review` must match"
        );
        assert!(
            is_reviewer_checkout("tmp/abc1234"),
            "`tmp/abc1234` must match (slash-separated tmp branch)"
        );
        assert!(
            is_reviewer_checkout("tmp-merge"),
            "`tmp-merge` must match (hyphen variant)"
        );
        assert!(is_reviewer_checkout("tmp"), "bare `tmp` must match");
    }

    /// `pr<N>_head` — the `gh pr fetch` / manual `git fetch origin
    /// refs/pull/<N>/head:pr<N>_head` style. Common operator-typed
    /// pattern when inspecting a PR locally. Must classify as
    /// reviewer_checkout.
    #[test]
    fn reviewer_checkout_pattern_matches_pr_head_suffix() {
        assert!(
            is_reviewer_checkout("pr123_head"),
            "`pr123_head` must match"
        );
        assert!(
            is_reviewer_checkout("pr850_head"),
            "`pr850_head` must match (real example from operator's report)"
        );
        assert!(
            is_reviewer_checkout("pr1_head"),
            "single-digit pr1_head must match"
        );
    }

    /// `review/.*` — explicit `review/<n>` namespace. Some workflows
    /// adopt this prefix for inspection refs.
    #[test]
    fn reviewer_checkout_pattern_matches_review_prefix() {
        assert!(
            is_reviewer_checkout("review/123"),
            "`review/123` must match"
        );
        assert!(
            is_reviewer_checkout("review/feat-x"),
            "`review/feat-x` must match"
        );
    }

    /// **CRITICAL** negative: legitimate working branch names must NOT
    /// match. The pattern is narrow by design — only the three
    /// observed pollution shapes. A false-positive here would have
    /// the boot sweeper auto-deleting legitimate work.
    #[test]
    fn reviewer_checkout_pattern_does_not_match_main_or_fix_branches() {
        assert!(!is_reviewer_checkout("main"), "main must NOT match");
        assert!(!is_reviewer_checkout("master"), "master must NOT match");
        assert!(
            !is_reviewer_checkout("fix/123-real-work"),
            "fix/.* (legitimate fix branch) must NOT match"
        );
        assert!(
            !is_reviewer_checkout("feat/some-feature"),
            "feat/.* must NOT match"
        );
        assert!(
            !is_reviewer_checkout("temporary-work"),
            "`temporary-work` must NOT match — only `tmp.*` (3-letter \
             prefix) qualifies, not arbitrary 'temp' variants"
        );
        assert!(
            !is_reviewer_checkout("pr-merge-queue"),
            "`pr-merge-queue` must NOT match — pattern requires \
             `pr\\d+_head` shape specifically"
        );
        assert!(
            !is_reviewer_checkout(""),
            "empty string must NOT match (defensive)"
        );
    }

    /// Spawn a temp git repo scoped to `tag`. The repo has an initial
    /// commit on `main` + pinned per-repo gitconfig (`user.name`/
    /// `user.email`) so subsequent git subprocess calls don't fail
    /// with "unable to auto-detect email address" under CI runners
    /// that lack a global ~/.gitconfig. Mirrors #814 r1's CI
    /// portability fix.
    ///
    /// Returns the repo dir path.
    pub(super) fn setup_repo(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("agend-817-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).ok();
        git_run(&repo, &["init", "-b", "main"]);
        git_run(&repo, &["config", "user.name", "test"]);
        git_run(&repo, &["config", "user.email", "t@t"]);
        git_run(&repo, &["commit", "--allow-empty", "-m", "main: initial"]);
        repo
    }

    /// Run git with predictable env. `GIT_AUTHOR_DATE` /
    /// `GIT_COMMITTER_DATE` callers use `git_run_dated` instead.
    pub(super) fn git_run(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran")
    }

    /// Run git with explicit author + committer date for back-dating
    /// commits. Used by stale_idle tests to plant commits N days in
    /// the past without `chrono::Utc::now() - duration` arithmetic
    /// (flaky near day boundaries).
    pub(super) fn git_run_dated(
        dir: &Path,
        args: &[&str],
        date_rfc3339: &str,
    ) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", date_rfc3339)
            .env("GIT_COMMITTER_DATE", date_rfc3339)
            .output()
            .expect("git ran")
    }

    /// Helper: create a branch off main with one commit. Returns the
    /// branch tip SHA.
    pub(super) fn create_branch_with_commit(repo: &Path, branch: &str, commit_msg: &str) -> String {
        git_run(repo, &["checkout", "-b", branch]);
        let file = repo.join(format!("{branch}.txt"));
        std::fs::write(&file, format!("content for {branch}\n")).expect("write");
        git_run(repo, &["add", &format!("{branch}.txt")]);
        git_run(repo, &["commit", "-m", commit_msg]);
        let sha = String::from_utf8_lossy(&git_run(repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(repo, &["checkout", "main"]);
        sha
    }

    fn bind_handler_repo(home: &Path, repo: &Path, agent: &str) {
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir binding");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");
    }

    fn handler_dry_run(home: &Path, repo: &Path, agent: &str) -> serde_json::Value {
        bind_handler_repo(home, repo, agent);
        crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            home,
            &serde_json::json!({"instance": agent}),
            agent,
        )
    }

    fn reviewer_candidate<'a>(
        response: &'a serde_json::Value,
        name: &str,
    ) -> &'a serde_json::Value {
        response["categories"]["reviewer_checkout"]
            .as_array()
            .expect("reviewer_checkout array")
            .iter()
            .find(|candidate| candidate["name"] == name)
            .unwrap_or_else(|| panic!("missing reviewer candidate {name}: {response}"))
    }

    fn add_local_bare_origin(repo: &Path) -> PathBuf {
        let origin = repo.parent().expect("repo parent").join("origin.git");
        git_run(
            repo,
            &["init", "--bare", origin.to_str().expect("origin path")],
        );
        git_run(
            repo,
            &[
                "remote",
                "add",
                "origin",
                origin.to_str().expect("origin path"),
            ],
        );
        origin
    }

    // PR-A RED: preservation evidence is returned through the real
    // cleanup_merged_branches dry-run handler. These assertions deliberately
    // use JSON fields so the RED commit compiles against the pre-feature
    // Candidate type and fails at the actual public response boundary.
    #[test]
    fn review_preservation_main_reachable_is_observability_only() {
        let repo = setup_repo("preservation_main");
        let home = repo.parent().unwrap().to_path_buf();
        add_local_bare_origin(&repo);
        git_run(&repo, &["branch", "tmp_main_reachable", "main"]);

        let response = handler_dry_run(&home, &repo, "preservation-main-agent");
        let candidate = reviewer_candidate(&response, "tmp_main_reachable");
        assert_eq!(
            candidate["preservation"]["classification"], "MAIN_REACHABLE",
            "main ancestry must be surfaced as current evidence: {response}"
        );
        assert_eq!(candidate["preservation"]["durable"], false);
        assert!(
            response["candidate_ids"]
                .as_array()
                .expect("candidate_ids")
                .iter()
                .any(|id| id == "tmp_main_reachable"),
            "PR-A is observability-only: classification must not remove the existing reviewer candidate"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn review_preservation_external_ancestor_uses_hermetic_pull_ref() {
        let repo = setup_repo("preservation_external");
        let home = repo.parent().unwrap().to_path_buf();
        add_local_bare_origin(&repo);
        let candidate_sha =
            create_branch_with_commit(&repo, "tmp_external", "review work under inspection");
        git_run(
            &repo,
            &["checkout", "-b", "external-descendant", &candidate_sha],
        );
        std::fs::write(repo.join("external-descendant.txt"), "sync commit\n").expect("write");
        git_run(&repo, &["add", "external-descendant.txt"]);
        git_run(&repo, &["commit", "-m", "external descendant"]);
        let descendant_sha =
            String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
                .trim()
                .to_string();
        git_run(
            &repo,
            &[
                "push",
                "origin",
                &format!("{descendant_sha}:refs/pull/7/head"),
            ],
        );
        git_run(&repo, &["checkout", "main"]);

        let response = handler_dry_run(&home, &repo, "preservation-external-agent");
        let candidate = reviewer_candidate(&response, "tmp_external");
        assert_eq!(
            candidate["preservation"]["classification"],
            "EXTERNALLY_REACHABLE_UNGUARANTEED",
            "candidate ancestor of a current pull head must be visible without claiming durability: {response}"
        );
        assert_eq!(candidate["preservation"]["durable"], false);

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn review_preservation_orphaned_reports_exact_unique_count() {
        let repo = setup_repo("preservation_orphan");
        let home = repo.parent().unwrap().to_path_buf();
        add_local_bare_origin(&repo); // successful, empty external inventory
        create_branch_with_commit(&repo, "tmp_orphan", "orphan commit one");
        git_run(&repo, &["checkout", "tmp_orphan"]);
        std::fs::write(repo.join("orphan-two.txt"), "second unique commit\n").expect("write");
        git_run(&repo, &["add", "orphan-two.txt"]);
        git_run(&repo, &["commit", "-m", "orphan commit two"]);
        git_run(&repo, &["checkout", "main"]);

        let response = handler_dry_run(&home, &repo, "preservation-orphan-agent");
        let candidate = reviewer_candidate(&response, "tmp_orphan");
        assert_eq!(
            candidate["preservation"]["classification"], "ORPHANED_UNIQUE",
            "orphan classification requires a successful external inventory: {response}"
        );
        assert_eq!(candidate["preservation"]["unique_commit_count"], 2);

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn review_preservation_external_failure_keeps_inventory_unknown() {
        let repo = setup_repo("preservation_unknown");
        let home = repo.parent().unwrap().to_path_buf();
        create_branch_with_commit(&repo, "tmp_unknown", "review work with offline origin");
        git_run(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "/definitely/missing/agend-origin.git",
            ],
        );

        let response = handler_dry_run(&home, &repo, "preservation-unknown-agent");
        assert_eq!(
            response["dry_run"], true,
            "local inventory must survive: {response}"
        );
        let candidate = reviewer_candidate(&response, "tmp_unknown");
        assert_eq!(
            candidate["preservation"]["classification"], "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
            "offline evidence must never fall through to ORPHANED: {response}"
        );
        assert_ne!(
            candidate["preservation"]["classification"],
            "ORPHANED_UNIQUE"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn review_preservation_unavailable_non_equal_external_object_is_unknown() {
        let repo = setup_repo("preservation_unavailable_object");
        let home = repo.parent().unwrap().to_path_buf();
        let origin = add_local_bare_origin(&repo);
        let candidate_sha =
            create_branch_with_commit(&repo, "tmp_unavailable", "locally available review tip");
        git_run(
            &repo,
            &[
                "push",
                "origin",
                &format!("{candidate_sha}:refs/heads/seed"),
            ],
        );

        // Create the external descendant in a separate clone, then remove the
        // seed ref. `ls-remote` can see the pull-head SHA, while the repository
        // being classified has never fetched that descendant object.
        let peer = repo.parent().unwrap().join("external-peer");
        git_run(
            &repo,
            &[
                "clone",
                origin.to_str().expect("origin path"),
                peer.to_str().expect("peer path"),
            ],
        );
        git_run(&peer, &["config", "user.name", "test"]);
        git_run(&peer, &["config", "user.email", "t@t"]);
        git_run(&peer, &["checkout", "seed"]);
        std::fs::write(peer.join("remote-only.txt"), "unfetched descendant\n").expect("write");
        git_run(&peer, &["add", "remote-only.txt"]);
        git_run(&peer, &["commit", "-m", "remote-only descendant"]);
        git_run(&peer, &["push", "origin", "HEAD:refs/pull/9/head"]);
        git_run(&repo, &["push", "origin", ":refs/heads/seed"]);

        let response = handler_dry_run(&home, &repo, "preservation-unavailable-agent");
        let candidate = reviewer_candidate(&response, "tmp_unavailable");
        assert_eq!(
            candidate["preservation"]["classification"], "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
            "a non-equal remote-only object cannot prove ancestry: {response}"
        );
        assert_ne!(
            candidate["preservation"]["classification"],
            "ORPHANED_UNIQUE"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn spike_residue_is_separate_annotation_not_candidate() {
        let repo = setup_repo("preservation_spike");
        let home = repo.parent().unwrap().to_path_buf();
        add_local_bare_origin(&repo);
        git_run(&repo, &["checkout", "-b", "spike/preservation-probe"]);
        std::fs::write(repo.join("spike-probe.txt"), "analysis artifact\n").expect("write");
        git_run(&repo, &["add", "spike-probe.txt"]);
        git_run(&repo, &["commit", "-m", "spike artifact"]);
        git_run(&repo, &["checkout", "main"]);

        let response = handler_dry_run(&home, &repo, "preservation-spike-agent");
        let annotations = response["annotations"]["spike_residue"]
            .as_array()
            .expect("separate spike_residue annotations");
        assert!(
            annotations.iter().any(|entry| {
                entry["name"] == "spike/preservation-probe"
                    && entry["annotation"] == "SPIKE_RESIDUE"
            }),
            "spike residue must be visible only as an annotation: {response}"
        );
        assert!(
            !response["candidate_ids"]
                .as_array()
                .expect("candidate_ids")
                .iter()
                .any(|id| id == "spike/preservation-probe"),
            "annotation must not add spike residue to candidate_ids"
        );
        assert!(
            !response["categories"]["reviewer_checkout"]
                .as_array()
                .expect("reviewer_checkout")
                .iter()
                .any(|candidate| candidate["name"] == "spike/preservation-probe"),
            "spike residue must remain outside reviewer_checkout"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_clean_merged() {
        // #817 RED 1: branch "feat-a" merged into main via a merge
        // commit lands in `clean_merged` (git branch --merged main
        // includes it). Stub returns empty Categories → assertion
        // fails. C2 lands the real scan that picks it up.
        let repo = setup_repo("clean_merged");
        create_branch_with_commit(&repo, "feat-a", "feat: a");
        // Merge via a no-fast-forward merge so a merge commit exists.
        git_run(&repo, &["merge", "--no-ff", "-m", "merge feat-a", "feat-a"]);
        // Branch still exists locally after merge.

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.clean_merged.iter().any(|c| c.name == "feat-a"),
            "clean_merged must include feat-a, got: {cats:?}"
        );
        // Not in other buckets.
        assert!(!cats.squash_merged.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.stale_idle.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.active_unknown.iter().any(|c| c.name == "feat-a"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_squash_merged() {
        // #817 RED 2: branch "feat-b" whose commit was squash-applied
        // to main as a NEW commit with same patch-id but DIFFERENT
        // SHA (mirrors GitHub's "Squash and merge" semantics). The
        // detector must use `git cherry main feat-b` (patch-id based)
        // — `git branch --merged` would miss this case because the
        // feat-b SHA isn't reachable from main HEAD.
        //
        // To simulate the SHA-divergence: main advances by an
        // unrelated commit FIRST, then we cherry-pick feat-b with
        // `--no-commit` + commit with a different message. The
        // resulting main HEAD has feat-b's patch but a fresh SHA.
        let repo = setup_repo("squash_merged");
        create_branch_with_commit(&repo, "feat-b", "feat: b body");
        // Make main diverge first so cherry-pick doesn't fast-forward.
        std::fs::write(repo.join("unrelated.txt"), "main moves\n").expect("write");
        git_run(&repo, &["add", "unrelated.txt"]);
        git_run(&repo, &["commit", "-m", "main: unrelated work"]);
        // Squash-apply feat-b's diff to main as a separate commit.
        git_run(&repo, &["cherry-pick", "--no-commit", "feat-b"]);
        git_run(&repo, &["commit", "-m", "squash: feat-b body"]);

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.squash_merged.iter().any(|c| c.name == "feat-b"),
            "squash_merged must include feat-b, got: {cats:?}"
        );
        // Not in clean_merged — feat-b's SHA is NOT in main's
        // ancestry post-squash (main has a different SHA with same
        // patch-id).
        assert!(!cats.clean_merged.iter().any(|c| c.name == "feat-b"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    // #2550 W6: cross-test pinning the behavior GAP between the
    // ancestor-check (`git merge-base --is-ancestor`, used by
    // worktree_cleanup.rs's is_branch_merged / worktree_pool.rs's
    // cleanup_merged_branch via git_helpers::git_ok) and this file's own
    // squash-merge detection (is_squash_merged, cherry + tree-diff based) —
    // for a GitHub-style squash-merged branch. This is the decision input
    // for whether the three merge-detection sites should unify onto
    // branch_sweep.rs's heavier squash-aware method.
    #[test]
    fn ancestor_check_misses_squash_merge_that_branch_sweep_catches() {
        let repo = setup_repo("w6_ancestor_vs_squash");
        create_branch_with_commit(&repo, "feat-c", "feat: c body");
        // Make main diverge first so cherry-pick doesn't fast-forward
        // (mirrors test_branch_sweep_scan_categorizes_squash_merged above).
        std::fs::write(repo.join("unrelated2.txt"), "main moves\n").expect("write");
        git_run(&repo, &["add", "unrelated2.txt"]);
        git_run(&repo, &["commit", "-m", "main: unrelated work 2"]);
        git_run(&repo, &["cherry-pick", "--no-commit", "feat-c"]);
        git_run(&repo, &["commit", "-m", "squash: feat-c body"]);

        assert!(
            !crate::git_helpers::git_ok(&repo, &["merge-base", "--is-ancestor", "feat-c", "main"],),
            "ancestor-check must return false for a squash-merged branch — \
             squash produces a new commit on main with no direct ancestry \
             back to feat-c's tip, so this is a real (not incidental) gap, \
             not a bug to unify away lightly"
        );
        assert!(
            is_squash_merged(&repo, "main", "feat-c"),
            "is_squash_merged (this file's cherry/patch-id detection) must \
             still catch it — this is the gap ancestor-check callers close \
             via a DIFFERENT, cheaper signal instead (remote-tracking-ref-gone,\
             see worktree_cleanup.rs's is_remote_gone / worktree_pool.rs's \
             is_gone), not by adopting this file's cherry+diff(+gh API) method"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    // t-20260704054810920172-67777-3: `local_sha_matches_merged_head`
    // regression coverage. main's now-default strict-up-to-date branch
    // protection means a required "Update branch" sync commit lands on the
    // remote HEAD before squash-merge, but this sweep's `fetch --prune`
    // never fast-forwards the local branch ref — so `local_sha` (this
    // sweep's only source of truth) stays one sync-commit behind the
    // merged PR's real `head_ref_oid` forever once the remote branch is
    // deleted. These pin the new is-ancestor acceptance without needing a
    // live GitHub API / ScmProvider mock — `local_sha_matches_merged_head`
    // takes both SHAs directly.

    #[test]
    fn local_sha_matches_merged_head_true_for_strict_ancestor_2637() {
        let repo = setup_repo("ancestor_true");
        let local_sha = create_branch_with_commit(&repo, "feat-d", "feat: d body");
        // Simulate the "Update branch" sync commit landing on the remote
        // HEAD after `local_sha` was last touched — one more commit on top,
        // never fetched back into the local branch ref.
        git_run(&repo, &["checkout", "feat-d"]);
        std::fs::write(repo.join("sync.txt"), "update-branch sync\n").expect("write");
        git_run(&repo, &["add", "sync.txt"]);
        git_run(&repo, &["commit", "-m", "sync with main"]);
        let head_ref_oid = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(&repo, &["checkout", "main"]);

        assert!(
            local_sha_matches_merged_head(&repo, &local_sha, &head_ref_oid),
            "local_sha strictly behind the actually-merged head_ref_oid (the \
             update-branch sync gap) must still match"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn local_sha_matches_merged_head_false_for_divergent_unmerged_commit_2637() {
        let repo = setup_repo("ancestor_false");
        let merged_base = create_branch_with_commit(&repo, "feat-e", "feat: e body");
        // `head_ref_oid`: what actually got merged (one commit past the
        // shared base).
        git_run(&repo, &["checkout", "feat-e"]);
        std::fs::write(repo.join("merged.txt"), "this landed in main\n").expect("write");
        git_run(&repo, &["add", "merged.txt"]);
        git_run(&repo, &["commit", "-m", "merged work"]);
        let head_ref_oid = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        // `local_sha`: a DIFFERENT, never-merged commit branched off the
        // same shared base — diverged, not an ancestor of head_ref_oid.
        git_run(&repo, &["checkout", "-b", "feat-e-local", &merged_base]);
        std::fs::write(repo.join("unmerged.txt"), "this never landed\n").expect("write");
        git_run(&repo, &["add", "unmerged.txt"]);
        git_run(&repo, &["commit", "-m", "unmerged local-only work"]);
        let local_sha = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(&repo, &["checkout", "main"]);

        assert!(
            !local_sha_matches_merged_head(&repo, &local_sha, &head_ref_oid),
            "a local_sha carrying real unmerged work outside head_ref_oid's \
             history must NOT match — this is the false-positive guard: \
             is-ancestor must never wrongly clear a branch with unpushed work"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn local_sha_matches_merged_head_true_for_exact_equal_2637() {
        let repo = setup_repo("ancestor_equal");
        let sha = create_branch_with_commit(&repo, "feat-f", "feat: f body");

        assert!(
            local_sha_matches_merged_head(&repo, &sha, &sha),
            "the pre-existing exact-SHA-match behavior must still hold"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn local_sha_matches_merged_head_false_when_head_ref_oid_unknown_locally_2637() {
        let repo = setup_repo("ancestor_missing_object");
        let local_sha = create_branch_with_commit(&repo, "feat-g", "feat: g body");

        assert!(
            !local_sha_matches_merged_head(
                &repo,
                &local_sha,
                "0000000000000000000000000000000000dead"
            ),
            "an is-ancestor check against an object git doesn't have locally \
             (e.g. a deleted remote branch's newest commit, never fetched) \
             must fail CLOSED — not treated as a match"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_stale_idle() {
        // #817 RED 3: branch "old-wip" with committer-date 100 days
        // in the past, not merged, not squash-merged → stale_idle.
        // Uses GIT_AUTHOR_DATE/COMMITTER_DATE env to back-date the
        // commit (NOT chrono arithmetic — flaky near day boundary).
        let repo = setup_repo("stale_idle");
        // Back-date by 100 days from a fixed reference point.
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old_date = (now - chrono::Duration::days(100)).to_rfc3339();
        git_run(&repo, &["checkout", "-b", "old-wip"]);
        std::fs::write(repo.join("wip.txt"), "wip content\n").expect("write");
        git_run(&repo, &["add", "wip.txt"]);
        git_run_dated(&repo, &["commit", "-m", "WIP: stale work"], &old_date);
        git_run(&repo, &["checkout", "main"]);

        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.stale_idle.iter().any(|c| c.name == "old-wip"),
            "stale_idle must include old-wip (100d > 90d threshold), got: {cats:?}"
        );
        // NOT merged + NOT squash-merged.
        assert!(!cats.clean_merged.iter().any(|c| c.name == "old-wip"));
        assert!(!cats.squash_merged.iter().any(|c| c.name == "old-wip"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    // ── #817 apply-path tests ──

    /// #2011 regression: a branch checked out in a worktree whose physical
    /// directory is GONE (crashed release / manual rm / pre-prune-era leak)
    /// must still be deletable by the sweep — git counts it "checked out"
    /// until the registration is pruned, and 14 such branches piled up live
    /// on 2026-06-11. emit_delete_batch now prunes orphaned registrations in
    /// the same transaction (delete dir → registration goes → branch
    /// deletable). Pre-#2011 this test fails: `branch -D` refuses with
    /// "checked out at".
    #[test]
    fn test_orphaned_worktree_registration_does_not_block_delete_2011() {
        let repo = setup_repo("orphan_wt_reg");
        let home = repo.parent().unwrap().to_path_buf();
        create_branch_with_commit(&repo, "feat-orphan", "feat: orphan");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-orphan", "feat-orphan"],
        );
        // Check the branch out in a worktree, then vaporize ONLY the
        // physical directory — the registration survives (the leak shape).
        let wt_dir = repo.parent().unwrap().join("orphan-wt-dir");
        std::fs::remove_dir_all(&wt_dir).ok(); // stale residue from a prior run
        let wt_str = wt_dir.display().to_string();
        let out = git_run(&repo, &["worktree", "add", &wt_str, "feat-orphan"]);
        assert!(
            out.status.success(),
            "worktree add must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_dir_all(&wt_dir).expect("rm worktree dir");
        // Precondition pin: the registration is still there (prunable).
        let list = git_run(&repo, &["worktree", "list", "--porcelain"]);
        assert!(
            String::from_utf8_lossy(&list.stdout).contains("orphan-wt-dir"),
            "leak shape precondition: registration must survive the rm"
        );

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        let mut confirm = std::collections::HashSet::new();
        confirm.insert("feat-orphan".to_string());
        let applied = emit_delete_batch(&home, &repo, &cats, &confirm, "#2011 test").expect("emit");
        assert_eq!(
            applied, 1,
            "orphaned registration must not block the branch delete"
        );
        let post = enumerate_branches(&repo).expect("enumerate");
        assert!(
            !post.iter().any(|b| b.name == "feat-orphan"),
            "feat-orphan must be deleted after the in-transaction prune"
        );
    }

    #[test]
    fn test_branch_sweep_apply_deletes_confirmed_subset() {
        // GREEN: emit_delete_batch runs `git branch -D <name>` for
        // each confirm_id and writes a `branch_sweep_apply` event-log
        // entry per success. Confirms double-opt-in actually deletes
        // the named branches AND records source SHA for restore.
        let repo = setup_repo("apply_subset");
        let home = repo.parent().unwrap().to_path_buf();
        // Create two clean-merged branches; only delete the first.
        create_branch_with_commit(&repo, "feat-keep", "feat: keep");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-keep", "feat-keep"],
        );
        create_branch_with_commit(&repo, "feat-delete", "feat: delete");
        git_run(
            &repo,
            &["merge", "--no-ff", "-m", "merge feat-delete", "feat-delete"],
        );

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert_eq!(
            cats.clean_merged.len(),
            2,
            "two branches expected: {cats:?}"
        );

        let mut confirm = std::collections::HashSet::new();
        confirm.insert("feat-delete".to_string());

        let applied =
            emit_delete_batch(&home, &repo, &cats, &confirm, "post-#817 test apply").expect("emit");
        assert_eq!(applied, 1, "exactly 1 deletion expected");

        // feat-delete is gone; feat-keep still exists.
        let post = enumerate_branches(&repo).expect("enumerate");
        let names: Vec<&str> = post.iter().map(|b| b.name.as_str()).collect();
        assert!(!names.contains(&"feat-delete"), "feat-delete must be gone");
        assert!(names.contains(&"feat-keep"), "feat-keep must remain");

        // Event-log entry per success.
        let log_path = home.join("event-log.jsonl");
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log.contains("branch_sweep_apply"),
            "event-log must record branch_sweep_apply, got: {log}"
        );
        assert!(
            log.contains("feat-delete"),
            "event-log must name the deleted branch"
        );
        assert!(
            log.contains("post-#817 test apply"),
            "event-log must carry the audit_reason"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_apply_skips_unknown_confirm_id() {
        // GREEN: emit_delete_batch tolerates confirm_ids that aren't
        // in any category (e.g. operator typo). Skips silently (the
        // handler-level validator rejects these BEFORE calling this
        // function, so emit_delete_batch's contract is "do best-effort
        // for the candidates it recognizes"). Returns 0 deletions.
        let repo = setup_repo("apply_skip_unknown");
        let home = repo.parent().unwrap().to_path_buf();
        let cats = Categories::default(); // empty
        let mut confirm = std::collections::HashSet::new();
        confirm.insert("nonexistent-branch".to_string());
        let applied =
            emit_delete_batch(&home, &repo, &cats, &confirm, "unknown probe").expect("emit");
        assert_eq!(
            applied, 0,
            "unknown confirm_ids yield 0 deletions, not errors"
        );
        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    /// t-20260704115315460591-14440-0 (#817 behavior gap): a `reviewer_checkout`
    /// candidate is NOT an unknown confirm_id — it's a recognized category, part
    /// of `deletable_ids()`'s own documented default-deletable list, and it
    /// survives the handler's `all_ids()` validation before ever reaching
    /// `emit_delete_batch`. Unlike `test_branch_sweep_apply_skips_unknown_
    /// confirm_id`'s intentional "typo confirm_id → silent no-op" contract,
    /// this must behave like `clean_merged`/`squash_merged`: real delete-or-log,
    /// never a silent no-op. Production incident (2026-07-04): dry-run listed
    /// 24 `review/*` branches as candidates, apply=true confirmed them, but
    /// `applied` didn't count them and no event-log entry appeared — the
    /// operator had no signal the branches survived.
    #[test]
    fn test_branch_sweep_apply_deletes_reviewer_checkout_candidate_2620() {
        let repo = setup_repo("apply_reviewer_checkout");
        let home = repo.parent().unwrap().to_path_buf();
        // Unmerged on purpose — reviewer_checkout is classified by NAME
        // pattern alone (scan()'s bucket 0, checked before merge-status),
        // so an unmerged residue branch must still be a REAL candidate.
        create_branch_with_commit(&repo, "tmp_pr_review", "reviewer checkout residue");

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert_eq!(
            cats.reviewer_checkout.len(),
            1,
            "precondition: review/123 must classify as reviewer_checkout: {cats:?}"
        );
        assert!(
            cats.deletable_ids().contains(&"tmp_pr_review".to_string()),
            "precondition: reviewer_checkout is in the default deletable list"
        );

        let mut confirm = std::collections::HashSet::new();
        confirm.insert("tmp_pr_review".to_string());
        let applied = emit_delete_batch(&home, &repo, &cats, &confirm, "reviewer-checkout probe")
            .expect("emit");

        assert_eq!(
            applied, 1,
            "a confirmed reviewer_checkout candidate must actually be deleted, \
             not silently dropped like an unrecognized confirm_id"
        );
        let post = enumerate_branches(&repo).expect("enumerate");
        assert!(
            !post.iter().any(|b| b.name == "tmp_pr_review"),
            "review/123 must actually be gone from the repo"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("branch_sweep_apply") && log.contains("tmp_pr_review"),
            "a real delete must leave the same audit trail as any other category, \
             not silence: {log}"
        );
        assert!(
            log.contains("category=reviewer_checkout"),
            "the audit trail must name the correct category, not fall through to \
             active_unknown: {log}"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_handler_apply_requires_audit_reason_and_confirm_ids() {
        // GREEN: handler validator rejects apply=true with missing
        // confirm_ids OR missing audit_reason. Sets up a minimal
        // binding so the handler can resolve source_repo.
        let repo = setup_repo("handler_validators");
        let home = repo.parent().unwrap().to_path_buf();
        let agent = "test-agent";
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");

        // apply=true without confirm_ids → reject.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({"instance": agent, "apply": true}),
            agent,
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("confirm_ids"))
                .unwrap_or(false),
            "missing confirm_ids must reject: {r}"
        );
        assert_eq!(r["code"], "missing_confirm_ids");

        // apply=true with confirm_ids but no audit_reason → reject.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": agent,
                "apply": true,
                "confirm_ids": ["some-branch"],
            }),
            agent,
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("audit_reason"))
                .unwrap_or(false),
            "missing audit_reason must reject: {r}"
        );
        assert_eq!(r["code"], "missing_audit_reason");

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_handler_active_unknown_requires_explicit_opt_in() {
        // GREEN: a branch in `active_unknown` (recent, unmerged, not
        // squash-applied) is NOT in `candidate_ids` (deletable_ids
        // excludes active_unknown). Handler's dry-run surfaces it
        // separately so the operator can SEE it. Operator can still
        // delete it by passing its name in confirm_ids — handler's
        // subset check uses all_ids (which DOES include
        // active_unknown). Locks the explicit-opt-in contract.
        let repo = setup_repo("active_unknown_opt_in");
        let home = repo.parent().unwrap().to_path_buf();
        let agent = "test-agent";
        let binding_dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&binding_dir).expect("mkdir");
        std::fs::write(
            binding_dir.join("binding.json"),
            serde_json::json!({
                "source_repo": repo.display().to_string(),
                "branch": "feature",
                "worktree": repo.display().to_string(),
            })
            .to_string(),
        )
        .expect("write binding");

        // Create a recent unmerged branch → active_unknown.
        create_branch_with_commit(&repo, "wip-active", "feat: active wip");

        // Dry-run: candidate_ids should be empty for wip-active
        // (only deletable buckets); active_unknown is in categories
        // but not in candidate_ids.
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({"instance": agent}),
            agent,
        );
        let candidate_ids: Vec<&str> = r["candidate_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !candidate_ids.contains(&"wip-active"),
            "wip-active must NOT be in candidate_ids (active_unknown opt-in), got: {candidate_ids:?}"
        );
        let active_unknown: Vec<&str> = r["categories"]["active_unknown"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(
            active_unknown.contains(&"wip-active"),
            "wip-active must appear in active_unknown bucket for visibility, got: {active_unknown:?}"
        );

        // Apply with wip-active in confirm_ids → handler accepts
        // (subset check uses all_ids, NOT deletable_ids).
        let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
            &home,
            &serde_json::json!({
                "instance": agent,
                "apply": true,
                "confirm_ids": ["wip-active"],
                "audit_reason": "explicit opt-in for active_unknown",
            }),
            agent,
        );
        assert_eq!(
            r["applied"], 1,
            "explicit confirm_ids opt-in must delete active_unknown branch: {r}"
        );

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }
}
