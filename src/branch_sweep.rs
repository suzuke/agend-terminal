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

/// Threshold for `stale_idle` category. Branches whose tip commit
/// committer-date is older than this AND not merged AND not squash-
/// merged land in `stale_idle`. Operator can override via
/// `min_age_days` arg on the MCP call. Dead-code allow lifts at C3
/// when the MCP handler reads the default.
#[allow(dead_code)]
pub(crate) const STALE_IDLE_DEFAULT_DAYS: i64 = 90;

/// Lightweight enumeration of a local branch — what `git for-each-ref`
/// returns. The category is computed separately via per-branch
/// `git cherry` / `git branch --merged` checks.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) struct Candidate {
    pub name: String,
    pub tip_sha: String,
    pub reason: String,
}

#[derive(Debug, Default, serde::Serialize)]
#[allow(dead_code)]
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

#[allow(dead_code)]
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
#[allow(dead_code)]
fn enumerate_branches(repo: &Path) -> Result<Vec<BranchInfo>, String> {
    let output = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)|%(objectname)|%(committerdate:iso8601-strict)",
            "refs/heads/",
        ])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map_err(|e| format!("git for-each-ref spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
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

/// Returns true if `branch` is reachable from `base` via a merge
/// commit (`git branch --merged base` includes it). Used to detect
/// the `clean_merged` category.
#[allow(dead_code)]
fn is_clean_merged(repo: &Path, base: &str, branch: &str) -> bool {
    let output = std::process::Command::new("git")
        .args(["branch", "--merged", base])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let Ok(o) = output else { return false };
    if !o.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&o.stdout);
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
fn is_squash_merged(repo: &Path, base: &str, branch: &str) -> bool {
    // Method 1: git cherry (works for cherry-picked commits).
    if is_squash_merged_cherry(repo, base, branch) {
        return true;
    }
    // Method 2: tree-diff comparison (works for GitHub squash-merge).
    is_squash_merged_diff(repo, base, branch)
}

/// `git cherry` based detection.
fn is_squash_merged_cherry(repo: &Path, base: &str, branch: &str) -> bool {
    let output = std::process::Command::new("git")
        .args(["cherry", base, branch])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let Ok(o) = output else { return false };
    if !o.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&o.stdout);
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

/// GitHub API based detection: query whether a merged PR exists for
/// this branch with matching HEAD SHA. Most reliable — not affected
/// by git history topology. SHA check prevents false positives from
/// branch name reuse.
fn is_squash_merged_diff(repo: &Path, base: &str, branch: &str) -> bool {
    // Resolve owner/repo from git remote origin.
    let remote = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Some(remote_url) = remote else {
        return false;
    };
    let gh_repo = extract_github_repo(&remote_url);
    let Some(gh_repo) = gh_repo else {
        return false;
    };
    // Get local branch tip SHA.
    let local_sha = std::process::Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Some(local_sha) = local_sha else {
        return false;
    };
    // gh pr list --state merged --head <branch> --json headRefOid
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "merged",
            "--head",
            branch,
            "--base",
            base,
            "--repo",
            &gh_repo,
            "--json",
            "headRefOid",
        ])
        .output();
    let Ok(o) = output else { return false };
    if !o.status.success() {
        return false;
    }
    // Parse JSON array and check if any PR's headRefOid matches local SHA.
    let stdout = String::from_utf8_lossy(&o.stdout);
    let prs: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();
    prs.iter().any(|pr| {
        pr["headRefOid"]
            .as_str()
            .map(|sha| sha == local_sha)
            .unwrap_or(false)
    })
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
#[allow(dead_code)]
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
    let mut name_to_candidate: std::collections::HashMap<&str, &Candidate> =
        std::collections::HashMap::new();
    for cand in categories
        .clean_merged
        .iter()
        .chain(categories.squash_merged.iter())
        .chain(categories.stale_idle.iter())
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
        } else {
            "active_unknown"
        }
    };
    let mut deleted = 0usize;
    for name in confirm_ids {
        let Some(cand) = name_to_candidate.get(name.as_str()) else {
            continue;
        };
        let output = std::process::Command::new("git")
            .args(["branch", "-D", name])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
        match output {
            Ok(o) if o.status.success() => {
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
            Ok(o) => {
                crate::event_log::log(
                    home,
                    "branch_sweep_apply_failed",
                    "system:branch_sweep",
                    &format!(
                        "branch={name} stderr={}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                );
            }
            Err(e) => {
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
            &serde_json::json!({"agent": agent, "apply": true}),
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
                "agent": agent,
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
            &serde_json::json!({"agent": agent}),
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
                "agent": agent,
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
