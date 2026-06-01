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
//! **PR-A is scaffold only: it defines the trait + GitHub impl + neutral
//! types and adds no callers.** The 10 `gh` sites stay as-is; conversion
//! is the subsequent PR-B… series. Everything here is therefore unused
//! in non-test builds until then — hence the module-wide `dead_code`
//! allow, which PR-B removes as it wires the first call sites.
#![allow(dead_code)]

use serde_json::Value;
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
    pub is_draft: Option<bool>,
    pub merged_at: Option<String>,
    pub merge_commit_oid: Option<String>,
    pub merge_state_status: Option<String>,
    pub files: Option<Vec<String>>,
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
    Failed { code: Option<i32>, stderr: String },
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
    /// `gh pr list …` → PRs matching `filter` (sites 7/9/10).
    fn pr_list(&self, repo: &str, filter: &ListFilter) -> anyhow::Result<Vec<PrSummary>>;
    /// `gh pr merge …` — the only WRITE (site 3).
    fn pr_merge(&self, repo: &str, pr: u64, opts: &MergeOpts) -> anyhow::Result<MergeOutcome>;
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

fn pr_list_args(repo: &str, filter: &ListFilter) -> Vec<String> {
    let mut a = vec!["pr".into(), "list".into(), "--repo".into(), repo.into()];
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
    a.push("--json".into());
    // Neutral superset of fields the three list sites read; callers pick
    // the subset they need off `PrSummary`.
    a.push("number,state,author,headRefName,headRefOid,isDraft,mergedAt".into());
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

/// Parse a `gh pr checks --json name,state` array.
fn parse_checks(v: &Value) -> Vec<CheckState> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(CheckState {
                        name: c["name"].as_str()?.to_string(),
                        state: c["state"].as_str()?.to_string(),
                    })
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

impl GitHubScmProvider {
    fn run(args: &[String]) -> anyhow::Result<std::process::Output> {
        Command::new("gh")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))
    }
}

impl ScmProvider for GitHubScmProvider {
    fn pr_view(&self, repo: &str, pr: u64, fields: &[&str]) -> anyhow::Result<PrSummary> {
        let out = Self::run(&pr_view_args(repo, pr, fields))?;
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
        let out = Self::run(&pr_checks_args(repo, pr))?;
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

    fn pr_list(&self, repo: &str, filter: &ListFilter) -> anyhow::Result<Vec<PrSummary>> {
        let out = Self::run(&pr_list_args(repo, filter))?;
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
        let out = Self::run(&pr_merge_args(repo, pr, opts))?;
        if out.status.success() {
            Ok(MergeOutcome::Submitted)
        } else {
            Ok(MergeOutcome::Failed {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
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
            fn pr_list(&self, _repo: &str, _filter: &ListFilter) -> anyhow::Result<Vec<PrSummary>> {
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
    fn pr_list_args_build_from_filter() {
        // site 9 (branch_sweep) shape: merged + head + base.
        let f = ListFilter {
            state: Some("merged"),
            head: Some("feat/x".into()),
            base: Some("main".into()),
            limit: None,
        };
        assert_eq!(
            pr_list_args("o/r", &f),
            vec![
                "pr",
                "list",
                "--repo",
                "o/r",
                "--state",
                "merged",
                "--head",
                "feat/x",
                "--base",
                "main",
                "--json",
                "number,state,author,headRefName,headRefOid,isDraft,mergedAt",
            ]
        );
        // site 10 (admin) shape: head + merged + limit 1.
        let f2 = ListFilter {
            state: Some("merged"),
            head: Some("feat/y".into()),
            base: None,
            limit: Some(1),
        };
        let a = pr_list_args("o/r", &f2);
        assert_eq!(a.last().map(String::as_str), Some("1"));
        assert!(a.contains(&"--limit".to_string()));
        assert!(!a.contains(&"--base".to_string()));
    }

    #[test]
    fn pr_merge_args_match_existing_gh_call() {
        // site 3: --admin --squash --delete-branch.
        assert_eq!(
            pr_merge_args(
                "o/r",
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
                "o/r",
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
    }

    // ---- parsers ----

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
    fn parse_checks_filters_malformed_entries() {
        let v = serde_json::json!([
            {"name": "build", "state": "SUCCESS"},
            {"name": "no_state"},                      // dropped (missing state)
            {"name": "lint", "state": "FAILURE"},
        ]);
        let checks = parse_checks(&v);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "build");
        assert_eq!(checks[1].state, "FAILURE");
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
