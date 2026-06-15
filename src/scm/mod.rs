//! ScmProvider — SCM pull-request operation abstraction (PR-A scaffold).
//!
//! Sibling to [`crate::daemon::ci_watch`]'s `CiProvider`: same shape
//! (provider-neutral typed returns, `detect_provider_from_remote` reuse,
//! non-GitHub → fail-loud `NotSupported`), but a **synchronous** trait —
//! every PR-op call site is a blocking caller (MCP handlers, sweep
//! threads, daemon ticks), unlike `CiProvider` which lives on the tokio
//! ci-runtime. Forcing async here would push `block_on` shims into all
//! 10 sites; sync keeps them unchanged. See `/tmp/scm-provider-spike.md`.
//!
//! Unlike `CiProvider` (HTTP/reqwest), `GitHubScmProvider` shells out to
//! the `gh` CLI — the method bodies are the existing inline `gh` blocks
//! moved in verbatim (byte-identical argv), which is what makes the
//! eventual call-site conversion behavior-preserving.
//!
//! Migration complete: PR-A scaffold; PR-B sites 7 (`pr_list`) + 4
//! (`pr_view`); PR-C sites 8 + 5 (`pr_view`) + 6 (`pr_checks`); PR-D
//! sites 1 (`pr_view`) + 2 (`pr_checks`) + 9 + 10 (`pr_list` + `cwd`);
//! **PR-Z site 3 (`pr_merge`, the only write)**. All four verbs and the
//! non-GitHub stubs (constructed by `make_scm_provider` for non-GitHub
//! remotes) are now reachable, so the module-wide `dead_code` allow is
//! gone.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Provider-neutral types (superset of what the 10 gh sites read).
// ---------------------------------------------------------------------------

/// A single PR, provider-neutral. Each field is `Option` because a given
/// `gh ... --json <fields>` call only populates the subset it requested;
/// the rest stay `None`. Mirrors `CiProvider`'s typed `CiRun`/`PrState`
/// returns (the lead-picked alternative to leaking raw `serde_json::Value`
/// into 10 callers).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PrSummary {
    pub number: u64,
    pub state: Option<String>,
    pub author_login: Option<String>,
    pub head_ref: Option<String>,
    pub head_ref_oid: Option<String>,
    /// #1750-B4: GitHub's `isCrossRepository` — true when the PR's head branch
    /// lives in a FORK, not the base repo. A cross-repo head_ref can collide
    /// with a base-repo branch name, so remote-orphan GC must never treat it as
    /// a base-repo branch to delete.
    pub is_cross_repository: Option<bool>,
    pub is_draft: Option<bool>,
    pub merged_at: Option<String>,
    pub merge_commit_oid: Option<String>,
    pub merge_state_status: Option<String>,
    pub files: Option<Vec<String>>,
}

/// A single issue, provider-neutral. Mirrors [`PrSummary`] — only the
/// `--json` subset requested is populated. For the #2061 task-sweep
/// `stale_open` category only `state` (OPEN vs CLOSED) is consulted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IssueSummary {
    pub number: u64,
    /// gh's `state`: "OPEN" | "CLOSED".
    pub state: Option<String>,
}

/// A single PR CI check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckState {
    pub name: String,
    pub state: String,
}

/// Filter for [`ScmProvider::pr_list`]. Mirrors the `gh pr list` flags the
/// three list sites (7/9/10) vary on.
#[derive(Debug, Clone, Default)]
pub(crate) struct ListFilter {
    /// `--state` (e.g. "all" | "merged" | "open"). `None` = gh default.
    pub state: Option<&'static str>,
    /// `--head <branch>`.
    pub head: Option<String>,
    /// `--base <branch>`.
    pub base: Option<String>,
    /// `--limit <n>`.
    pub limit: Option<u32>,
}

/// Options for [`ScmProvider::pr_merge`] (site 3).
#[derive(Debug, Clone, Default)]
pub(crate) struct MergeOpts {
    pub admin: bool,
    pub squash: bool,
    pub delete_branch: bool,
}

/// Result of [`ScmProvider::pr_merge`]. `Submitted` means `gh pr merge`
/// exited 0 — NECESSARY but not SUFFICIENT for a landing (the caller
/// still runs the `verify_merge_landed` post-condition; that retry loop
/// is deliberately NOT folded into the trait — see #1467 / spike §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    Submitted,
    Failed { stderr: String },
}

/// #2140: result of [`ScmProvider::compare`] — DETERMINISTIC commit-ancestry for
/// the merge-freshness gate, independent of GitHub's eventually-consistent
/// `mergeStateStatus`. `behind_by` = how many commits `base` has that `head`
/// lacks (0 ⇒ up-to-date); `files` = the paths in the `base...head` symmetric diff
/// (the changes on `head` since the merge-base).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompareResult {
    pub behind_by: u64,
    pub files: Vec<String>,
}

/// Returned by non-GitHub providers so callers fail LOUD instead of an
/// auto-merge silently no-opping (§3.7 cross-backend stance). Hand-rolled
/// (no `thiserror` dependency) — it composes into `anyhow::Error` via the
/// blanket `std::error::Error` conversion.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NotSupported(pub &'static str);

impl std::fmt::Display for NotSupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SCM provider '{}' does not support PR operations (GitHub-only)",
            self.0
        )
    }
}

impl std::error::Error for NotSupported {}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over an SCM host's pull-request operations. The 10
/// hardcoded `gh` sites collapse onto these four verbs (everything else
/// is parsing). Sync by design (see module docs).
pub(crate) trait ScmProvider: Send + Sync {
    /// `gh pr view <pr> --json <fields>` → one PR (sites 1/4/5/8).
    fn pr_view(&self, repo: &str, pr: u64, fields: &[&str]) -> anyhow::Result<PrSummary>;
    /// `gh pr checks <pr> --json name,state` (sites 2/6).
    fn pr_checks(&self, repo: &str, pr: u64) -> anyhow::Result<Vec<CheckState>>;
    /// `gh pr list …` → PRs matching `filter` (sites 7/9/10). `fields`
    /// is the explicit `--json` set (each list site reads a different
    /// subset), passed verbatim.
    ///
    /// `cwd`: when `Some(dir)`, gh runs in `dir` and `--repo` is OMITTED
    /// (gh auto-detects the repo from the cwd's remote) — site 10
    /// (`admin::has_merged_pr`) has only a filesystem path, no slug, and
    /// relied on this. When `None`, `--repo <repo>` is emitted as usual.
    fn pr_list(
        &self,
        repo: &str,
        filter: &ListFilter,
        fields: &[&str],
        cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>>;
    /// `gh pr merge …` — the only WRITE (site 3).
    fn pr_merge(&self, repo: &str, pr: u64, opts: &MergeOpts) -> anyhow::Result<MergeOutcome>;
    /// `gh issue view <number> --json <fields>` → one issue (#2061 task-sweep
    /// `stale_open`: resolve whether a referenced issue is CLOSED/terminal).
    fn issue_view(&self, repo: &str, number: u64, fields: &[&str]) -> anyhow::Result<IssueSummary>;
    /// #2140: `gh api repos/{repo}/compare/{base}...{head}` → DETERMINISTIC
    /// commit-ancestry (`behind_by` + changed `files`) for the merge-freshness
    /// gate, independent of the laggy `mergeStateStatus`.
    fn compare(&self, repo: &str, base: &str, head: &str) -> anyhow::Result<CompareResult>;
}

// ---------------------------------------------------------------------------
// argv builders — pure + unit-tested. Pinning the exact `gh` argv here is
// what PR-B's byte-identical conversions assert against.
// ---------------------------------------------------------------------------

fn pr_view_args(repo: &str, pr: u64, fields: &[&str]) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        fields.join(","),
    ]
}

fn pr_checks_args(repo: &str, pr: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "checks".into(),
        pr.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        "name,state".into(),
    ]
}

fn pr_list_args(
    repo: &str,
    filter: &ListFilter,
    fields: &[&str],
    cwd: Option<&Path>,
) -> Vec<String> {
    // Canonical order: `pr list [--repo R] --json <fields> [--state]
    // [--head] [--base] [--limit]`. Per decision d-20260601151209762922-0,
    // byte-identical means the same flags + values (a SET); gh treats flag
    // ORDER as insensitive, so the list sites whose original order differs
    // (site 9) stay behavior-identical — their pins assert set-equality.
    // When `cwd` is Some, `--repo` is omitted (gh auto-detects from the
    // cwd's remote — site 10's exact pre-conversion argv had no `--repo`).
    let mut a = vec!["pr".into(), "list".into()];
    if cwd.is_none() {
        a.push("--repo".into());
        a.push(repo.into());
    }
    a.push("--json".into());
    a.push(fields.join(","));
    if let Some(s) = filter.state {
        a.push("--state".into());
        a.push(s.into());
    }
    if let Some(h) = &filter.head {
        a.push("--head".into());
        a.push(h.clone());
    }
    if let Some(b) = &filter.base {
        a.push("--base".into());
        a.push(b.clone());
    }
    if let Some(l) = filter.limit {
        a.push("--limit".into());
        a.push(l.to_string());
    }
    a
}

fn pr_merge_args(repo: &str, pr: u64, opts: &MergeOpts) -> Vec<String> {
    let mut a = vec![
        "pr".into(),
        "merge".into(),
        pr.to_string(),
        "--repo".into(),
        repo.into(),
    ];
    if opts.admin {
        a.push("--admin".into());
    }
    if opts.squash {
        a.push("--squash".into());
    }
    if opts.delete_branch {
        a.push("--delete-branch".into());
    }
    a
}

fn issue_view_args(repo: &str, number: u64, fields: &[&str]) -> Vec<String> {
    vec![
        "issue".into(),
        "view".into(),
        number.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        fields.join(","),
    ]
}

/// #2140: `gh api repos/{repo}/compare/{base}...{head}` argv. The triple-dot
/// (`...`) yields the symmetric merge-base comparison (`behind_by` = commits
/// `base` has that `head` lacks; `files` = `head`'s changes since the merge-base).
fn compare_args(repo: &str, base: &str, head: &str) -> Vec<String> {
    vec![
        "api".into(),
        format!("repos/{repo}/compare/{base}...{head}"),
    ]
}

// ---------------------------------------------------------------------------
// parsers — pure + unit-tested. All gh-schema knowledge lives here, never
// in the call sites.
// ---------------------------------------------------------------------------

/// Parse one `gh pr view`/`gh pr list` JSON object into a [`PrSummary`].
/// Reads whatever keys are present (the `--json` field set decides which);
/// absent keys stay `None`. Empty-string `mergedAt`/`mergeCommit.oid` are
/// normalized to `None` (gh emits "" for not-yet-merged PRs).
fn parse_pr_summary(v: &Value) -> PrSummary {
    let nonempty = |s: &str| -> Option<String> {
        let t = v[s].as_str().filter(|x| !x.is_empty());
        t.map(String::from)
    };
    PrSummary {
        number: v["number"].as_u64().unwrap_or(0),
        state: v["state"].as_str().map(String::from),
        author_login: v["author"]["login"].as_str().map(String::from),
        head_ref: v["headRefName"].as_str().map(String::from),
        head_ref_oid: v["headRefOid"].as_str().map(String::from),
        is_cross_repository: v["isCrossRepository"].as_bool(),
        is_draft: v["isDraft"].as_bool(),
        merged_at: nonempty("mergedAt"),
        merge_commit_oid: v["mergeCommit"]["oid"]
            .as_str()
            .filter(|x| !x.is_empty())
            .map(String::from),
        merge_state_status: v["mergeStateStatus"].as_str().map(String::from),
        files: v["files"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|f| f["path"].as_str().map(String::from))
                .collect()
        }),
    }
}

/// Parse one `gh issue view --json …` JSON object into an [`IssueSummary`].
fn parse_issue_summary(v: &Value) -> IssueSummary {
    IssueSummary {
        number: v["number"].as_u64().unwrap_or(0),
        state: v["state"].as_str().map(String::from),
    }
}

/// #2140: parse `gh api .../compare/...` → `behind_by` + the changed file paths
/// (`files[].filename`). A missing field defaults to `0` / empty.
fn parse_compare(v: &Value) -> CompareResult {
    CompareResult {
        behind_by: v["behind_by"].as_u64().unwrap_or(0),
        files: v["files"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f["filename"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Parse a `gh pr checks --json name,state` array.
///
/// #PR-C: every array element is kept (NO drop on a missing/null `name`
/// or `state`) — a null/absent `state` becomes `""`. This is required so
/// the site-6 (`check_ci_green`) count reproduces the prior `--jq`
/// `select(.state != "SUCCESS" and .state != "SKIPPED")` exactly, where a
/// null state is `!= "SUCCESS"` ⟹ counted as not-passed (fail-closed). It
/// also matches site 2's `as_str().unwrap_or("")` treatment of null state.
fn parse_checks(v: &Value) -> Vec<CheckState> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| CheckState {
                    name: c["name"].as_str().unwrap_or("").to_string(),
                    state: c["state"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GitHub implementation — shells out to `gh` (bodies moved from the inline
// sites verbatim; the argv/parse helpers above are the byte-identical seam).
// ---------------------------------------------------------------------------

/// GitHub `gh` CLI implementation of [`ScmProvider`]. Unit struct — `gh`
/// carries its own auth (env / `gh auth`) and host, like the inline sites.
pub(crate) struct GitHubScmProvider;

/// CR-2026-06-14 #5 (perf): network bound for the `gh` CLI. `gh` is reached from
/// the per-tick worktree-cleanup sweep (`is_squash_merged` → `pr_list`), so a
/// slow/hanging gh (network stall, auth prompt, rate-limit retry) must not block
/// the daemon cleanup thread forever. A healthy `gh pr list/view` is a few
/// seconds; 60s is generous headroom that never false-kills a legit call yet
/// fails fast instead of hanging. (Purpose-named at this call site, per the
/// `git_helpers` note left when the old 300s `NETWORK_GIT_TIMEOUT` was removed.)
const GH_CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl GitHubScmProvider {
    /// Run `gh` with the given args. `cwd`: when `Some`, set the process
    /// working directory (site 10 runs gh inside the repo dir so gh
    /// auto-detects the repo, no `--repo`).
    fn run(args: &[String], cwd: Option<&Path>) -> anyhow::Result<std::process::Output> {
        let mut cmd = Command::new("gh");
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // CR-2026-06-14 #5: bound the `gh` subprocess via the shared
        // process-group-killing spawner (same machinery as the local git
        // helpers) — a bare `.output()` is UNBOUNDED and a wedged gh hangs the
        // per-tick cleanup sweep. The fast path is byte-identical (same captured
        // `Output`); on the deadline the gh process group is killed and the
        // caller gets `Err(TimedOut)` to fail fast instead of blocking.
        crate::git_helpers::spawn_group_bounded(
            cmd,
            &format!("gh {:?}", args.first()),
            GH_CLI_TIMEOUT,
        )
        .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))
    }
}

impl ScmProvider for GitHubScmProvider {
    fn pr_view(&self, repo: &str, pr: u64, fields: &[&str]) -> anyhow::Result<PrSummary> {
        let out = Self::run(&pr_view_args(repo, pr, fields), None)?;
        if !out.status.success() {
            anyhow::bail!(
                "gh pr view #{pr} ({repo}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(parse_pr_summary(&v))
    }

    fn pr_checks(&self, repo: &str, pr: u64) -> anyhow::Result<Vec<CheckState>> {
        let out = Self::run(&pr_checks_args(repo, pr), None)?;
        if !out.status.success() {
            anyhow::bail!(
                "gh pr checks #{pr} ({repo}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(parse_checks(&v))
    }

    fn pr_list(
        &self,
        repo: &str,
        filter: &ListFilter,
        fields: &[&str],
        cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        let out = Self::run(&pr_list_args(repo, filter, fields, cwd), cwd)?;
        if !out.status.success() {
            anyhow::bail!(
                "gh pr list ({repo}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(v.as_array()
            .map(|arr| arr.iter().map(parse_pr_summary).collect())
            .unwrap_or_default())
    }

    fn pr_merge(&self, repo: &str, pr: u64, opts: &MergeOpts) -> anyhow::Result<MergeOutcome> {
        let out = Self::run(&pr_merge_args(repo, pr, opts), None)?;
        if out.status.success() {
            Ok(MergeOutcome::Submitted)
        } else {
            Ok(MergeOutcome::Failed {
                // #PR-Z: NOT trimmed — site 3 (the sole caller) put the raw
                // stderr in its error JSON (`.to_string()`); preserve that
                // byte-for-byte.
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            })
        }
    }

    fn issue_view(&self, repo: &str, number: u64, fields: &[&str]) -> anyhow::Result<IssueSummary> {
        let out = Self::run(&issue_view_args(repo, number, fields), None)?;
        if !out.status.success() {
            anyhow::bail!(
                "gh issue view #{number} ({repo}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(parse_issue_summary(&v))
    }

    fn compare(&self, repo: &str, base: &str, head: &str) -> anyhow::Result<CompareResult> {
        let out = Self::run(&compare_args(repo, base, head), None)?;
        if !out.status.success() {
            anyhow::bail!(
                "gh api compare {base}...{head} ({repo}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(parse_compare(&v))
    }
}

// ---------------------------------------------------------------------------
// Non-GitHub stubs — fail loud (no silent no-op). Real impls are out of
// scope (spike non-goal); they exist so `make_scm_provider` can route a
// non-GitHub remote to an explicit `NotSupported` instead of a panic.
// ---------------------------------------------------------------------------

macro_rules! not_supported_provider {
    ($name:ident, $kind:literal) => {
        pub(crate) struct $name;
        impl ScmProvider for $name {
            fn pr_view(
                &self,
                _repo: &str,
                _pr: u64,
                _fields: &[&str],
            ) -> anyhow::Result<PrSummary> {
                Err(NotSupported($kind).into())
            }
            fn pr_checks(&self, _repo: &str, _pr: u64) -> anyhow::Result<Vec<CheckState>> {
                Err(NotSupported($kind).into())
            }
            fn pr_list(
                &self,
                _repo: &str,
                _filter: &ListFilter,
                _fields: &[&str],
                _cwd: Option<&Path>,
            ) -> anyhow::Result<Vec<PrSummary>> {
                Err(NotSupported($kind).into())
            }
            fn pr_merge(
                &self,
                _repo: &str,
                _pr: u64,
                _opts: &MergeOpts,
            ) -> anyhow::Result<MergeOutcome> {
                Err(NotSupported($kind).into())
            }
            fn issue_view(
                &self,
                _repo: &str,
                _number: u64,
                _fields: &[&str],
            ) -> anyhow::Result<IssueSummary> {
                Err(NotSupported($kind).into())
            }
            fn compare(
                &self,
                _repo: &str,
                _base: &str,
                _head: &str,
            ) -> anyhow::Result<CompareResult> {
                Err(NotSupported($kind).into())
            }
        }
    };
}

not_supported_provider!(GitLabScmProvider, "gitlab");
not_supported_provider!(BitbucketScmProvider, "bitbucket");

// ---------------------------------------------------------------------------
// Selection — reuse ci_watch's `detect_provider_from_remote` so SCM and CI
// provider detection stay consistent. `scm_override` mirrors a future
// fleet.yaml `scm_provider:` field (the optional explicit override the
// spike notes is rarely set — not plumbed through call sites in PR-A).
// ---------------------------------------------------------------------------

pub(crate) fn make_scm_provider(repo: &str, scm_override: Option<&str>) -> Box<dyn ScmProvider> {
    let kind = match scm_override {
        Some(k) => k,
        None => crate::daemon::ci_watch::detect_provider_from_remote(repo).0,
    };
    match kind {
        "gitlab" => Box::new(GitLabScmProvider),
        "bitbucket_cloud" | "bitbucket_server" | "bitbucket" => Box::new(BitbucketScmProvider),
        // `detect_provider_from_remote` already defaults unknown/short-form
        // remotes to "github", so this arm is GitHub + any explicit "github".
        _ => Box::new(GitHubScmProvider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- argv builders (byte-identical pins for PR-B conversions) ----

    #[test]
    fn pr_view_args_match_existing_gh_call() {
        // site 1 (verify_merge_landed) field set.
        assert_eq!(
            pr_view_args(
                "suzuke/agend-terminal",
                1467,
                &["state", "mergeCommit", "mergedAt", "mergeStateStatus"]
            ),
            vec![
                "pr",
                "view",
                "1467",
                "--repo",
                "suzuke/agend-terminal",
                "--json",
                "state,mergeCommit,mergedAt,mergeStateStatus",
            ]
        );
    }

    #[test]
    fn pr_view_args_prc_sites_drop_q_and_jq() {
        // #PR-C: sites 8 + 5 previously used gh's `-q`/`--jq` to extract a
        // field server-side. pr_view intentionally does NOT emit those —
        // it returns the parsed PrSummary field instead. These pins make
        // the (intentional, behavior-identical) argv delta explicit.
        // site 8 (sha_gate): was `... --json headRefOid -q .headRefOid`.
        assert_eq!(
            pr_view_args("o/r", 1, &["headRefOid"]),
            vec!["pr", "view", "1", "--repo", "o/r", "--json", "headRefOid"]
        );
        // site 5 (task_sweep): was `... --json files --jq .files[].path`.
        assert_eq!(
            pr_view_args("o/r", 1, &["files"]),
            vec!["pr", "view", "1", "--repo", "o/r", "--json", "files"]
        );
    }

    #[test]
    fn pr_checks_args_match_existing_gh_call() {
        assert_eq!(
            pr_checks_args("o/r", 42),
            vec![
                "pr",
                "checks",
                "42",
                "--repo",
                "o/r",
                "--json",
                "name,state"
            ]
        );
    }

    #[test]
    fn pr_list_args_match_site7_byte_identical() {
        // #PR-B byte-identical anchor: site 7 (pr_state/gh_poll.rs
        // CliGhPoller::poll) emits EXACTLY this argv pre-conversion —
        // `gh pr list --repo R --json <6 fields> --state all --limit 100`.
        let f = ListFilter {
            state: Some("all"),
            head: None,
            base: None,
            limit: Some(100),
        };
        assert_eq!(
            pr_list_args(
                "suzuke/agend-terminal",
                &f,
                &[
                    "author",
                    "number",
                    "headRefName",
                    "isDraft",
                    "state",
                    "mergedAt"
                ],
                None,
            ),
            vec![
                "pr",
                "list",
                "--repo",
                "suzuke/agend-terminal",
                "--json",
                "author,number,headRefName,isDraft,state,mergedAt",
                "--state",
                "all",
                "--limit",
                "100",
            ]
        );
    }

    #[test]
    fn pr_list_args_optional_flags_omitted_when_unset() {
        // head/base/state/limit each appear only when set; fields are
        // passed verbatim (each list site supplies its own subset).
        let f = ListFilter {
            state: Some("merged"),
            head: Some("feat/x".into()),
            base: Some("main".into()),
            limit: None,
        };
        let a = pr_list_args("o/r", &f, &["headRefOid"], None);
        assert_eq!(
            a,
            vec![
                "pr",
                "list",
                "--repo",
                "o/r",
                "--json",
                "headRefOid",
                "--state",
                "merged",
                "--head",
                "feat/x",
                "--base",
                "main",
            ]
        );
        assert!(!a.contains(&"--limit".to_string()));
    }

    #[test]
    fn pr_list_args_site9_set_equality_with_original() {
        // #PR-D site 9 (branch_sweep): the prior inline argv was
        //   pr list --state merged --head B --base BASE --repo R --json headRefOid
        // Our canonical builder emits the SAME flags+values in a different
        // ORDER (gh order-insensitive). Per decision d-20260601151209762922-0
        // byte-identical = set-equality (flags+values), not exact sequence —
        // so assert the multiset of tokens matches.
        let f = ListFilter {
            state: Some("merged"),
            head: Some("feat/dedup".into()),
            base: Some("main".into()),
            limit: None,
        };
        let mut produced = pr_list_args("suzuke/agend-terminal", &f, &["headRefOid"], None);
        let mut original: Vec<String> = [
            "pr",
            "list",
            "--state",
            "merged",
            "--head",
            "feat/dedup",
            "--base",
            "main",
            "--repo",
            "suzuke/agend-terminal",
            "--json",
            "headRefOid",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        produced.sort();
        original.sort();
        assert_eq!(
            produced, original,
            "site-9 argv must be set-equal to the original (flags+values), order aside"
        );
    }

    #[test]
    fn pr_list_args_cwd_some_omits_repo() {
        // #PR-D site 10 (admin::has_merged_pr): ran gh in the repo dir with
        // NO --repo (gh auto-detects). cwd=Some ⟹ `--repo` is omitted; the
        // argv is byte-identical (set) to the prior `pr list --head B
        // --state merged --json number --limit 1`.
        let f = ListFilter {
            state: Some("merged"),
            head: Some("feat/x".into()),
            base: None,
            limit: Some(1),
        };
        let a = pr_list_args(
            "ignored-when-cwd",
            &f,
            &["number"],
            Some(Path::new("/repo")),
        );
        assert!(
            !a.contains(&"--repo".to_string()),
            "cwd=Some must omit --repo (gh auto-detects from the cwd)"
        );
        let mut produced = a;
        let mut original: Vec<String> = [
            "pr", "list", "--head", "feat/x", "--state", "merged", "--json", "number", "--limit",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        produced.sort();
        original.sort();
        assert_eq!(
            produced, original,
            "site-10 argv set-equal to original (no --repo)"
        );
    }

    #[test]
    fn pr_merge_args_match_existing_gh_call() {
        // #PR-Z site-3 BYTE-IDENTICAL anchor: handle_merge_repo's only write
        // emitted EXACTLY `gh pr merge <pr> --repo R --admin --squash
        // --delete-branch` (verified via git show — flag ORDER is
        // admin→squash→delete-branch, NOT reordered).
        assert_eq!(
            pr_merge_args(
                "suzuke/agend-terminal",
                7,
                &MergeOpts {
                    admin: true,
                    squash: true,
                    delete_branch: true,
                }
            ),
            vec![
                "pr",
                "merge",
                "7",
                "--repo",
                "suzuke/agend-terminal",
                "--admin",
                "--squash",
                "--delete-branch",
            ]
        );
        // No-flag merge omits the optional flags.
        assert_eq!(
            pr_merge_args("o/r", 7, &MergeOpts::default()),
            vec!["pr", "merge", "7", "--repo", "o/r"]
        );
        // Per-flag MergeOpts → argv mapping (each flag independent).
        assert_eq!(
            pr_merge_args(
                "o/r",
                1,
                &MergeOpts {
                    admin: true,
                    ..Default::default()
                }
            ),
            vec!["pr", "merge", "1", "--repo", "o/r", "--admin"]
        );
        assert_eq!(
            pr_merge_args(
                "o/r",
                1,
                &MergeOpts {
                    squash: true,
                    ..Default::default()
                }
            ),
            vec!["pr", "merge", "1", "--repo", "o/r", "--squash"]
        );
        assert_eq!(
            pr_merge_args(
                "o/r",
                1,
                &MergeOpts {
                    delete_branch: true,
                    ..Default::default()
                }
            ),
            vec!["pr", "merge", "1", "--repo", "o/r", "--delete-branch"]
        );
    }

    #[test]
    fn issue_view_args_match_gh_issue_view_call() {
        // #2061 task-sweep stale_open: `gh issue view <n> --repo R --json state`.
        assert_eq!(
            issue_view_args("suzuke/agend-terminal", 2061, &["state"]),
            vec![
                "issue",
                "view",
                "2061",
                "--repo",
                "suzuke/agend-terminal",
                "--json",
                "state",
            ]
        );
    }

    // ---- parsers ----

    #[test]
    fn parse_issue_summary_reads_state() {
        let closed = parse_issue_summary(&serde_json::json!({"number": 2061, "state": "CLOSED"}));
        assert_eq!(closed.number, 2061);
        assert_eq!(closed.state.as_deref(), Some("CLOSED"));
        let open = parse_issue_summary(&serde_json::json!({"number": 7, "state": "OPEN"}));
        assert_eq!(open.state.as_deref(), Some("OPEN"));
        // Absent state stays None (caller treats as Unknown → not terminal).
        let bare = parse_issue_summary(&serde_json::json!({"number": 1}));
        assert_eq!(bare.state, None);
    }

    #[test]
    fn parse_pr_summary_reads_present_fields() {
        let v = serde_json::json!({
            "number": 1467,
            "state": "MERGED",
            "author": {"login": "octocat"},
            "headRefName": "feat/x",
            "headRefOid": "abc123",
            "isDraft": false,
            "mergedAt": "2026-06-01T00:00:00Z",
            "mergeCommit": {"oid": "def456"},
            "mergeStateStatus": "CLEAN",
            "files": [{"path": "src/a.rs"}, {"path": "src/b.rs"}],
        });
        let s = parse_pr_summary(&v);
        assert_eq!(s.number, 1467);
        assert_eq!(s.state.as_deref(), Some("MERGED"));
        assert_eq!(s.author_login.as_deref(), Some("octocat"));
        assert_eq!(s.head_ref_oid.as_deref(), Some("abc123"));
        assert_eq!(s.is_draft, Some(false));
        assert_eq!(s.merge_commit_oid.as_deref(), Some("def456"));
        assert_eq!(
            s.files,
            Some(vec!["src/a.rs".to_string(), "src/b.rs".to_string()])
        );
    }

    #[test]
    fn parse_pr_summary_empty_merge_fields_become_none() {
        // gh emits "" for mergedAt / mergeCommit.oid on an unmerged PR.
        let v = serde_json::json!({
            "number": 5,
            "state": "OPEN",
            "mergedAt": "",
            "mergeCommit": {"oid": ""},
        });
        let s = parse_pr_summary(&v);
        assert_eq!(s.merged_at, None);
        assert_eq!(s.merge_commit_oid, None);
        // Absent fields stay None too.
        assert_eq!(s.author_login, None);
        assert_eq!(s.files, None);
    }

    #[test]
    fn parse_checks_keeps_all_entries_null_state_empty() {
        // #PR-C: NO drop — a missing/null `state` is kept as "" so the
        // site-6 fail-closed count treats it as not-passed (matches the
        // prior `--jq` select on null state).
        let v = serde_json::json!([
            {"name": "build", "state": "SUCCESS"},
            {"name": "no_state"},          // kept, state → ""
            {"name": "lint", "state": "FAILURE"},
            {"state": "PENDING"},          // kept, name → ""
        ]);
        let checks = parse_checks(&v);
        assert_eq!(checks.len(), 4, "every array element is kept");
        assert_eq!(checks[0].name, "build");
        assert_eq!(
            checks[1].state, "",
            "missing state → empty string (not dropped)"
        );
        assert_eq!(checks[2].state, "FAILURE");
        assert_eq!(checks[3].name, "", "missing name → empty string");
    }

    // ---- selection + NotSupported ----

    #[test]
    fn make_scm_provider_routes_non_github_to_not_supported() {
        // GitHub remote → GitHubScmProvider (no NotSupported).
        let gh = make_scm_provider("suzuke/agend-terminal", None);
        // GitLab remote → stub that fails loud.
        let gl = make_scm_provider("https://gitlab.com/o/r", None);
        let err = gl
            .pr_merge("o/r", 1, &MergeOpts::default())
            .expect_err("non-GitHub pr_merge must fail loud");
        assert!(
            err.to_string().contains("does not support"),
            "expected NotSupported, got: {err}"
        );
        // Explicit override wins over detection.
        let forced = make_scm_provider("suzuke/agend-terminal", Some("gitlab"));
        assert!(forced.pr_view("o/r", 1, &["state"]).is_err());
        // GitHub provider's pr_view does NOT short-circuit as NotSupported
        // (it would attempt a gh call) — assert it's a different provider by
        // confirming it is constructed (smoke: trait object dispatch works).
        let _ = gh;
    }

    #[test]
    fn not_supported_display_names_the_provider() {
        assert_eq!(
            NotSupported("gitlab").to_string(),
            "SCM provider 'gitlab' does not support PR operations (GitHub-only)"
        );
    }
}
