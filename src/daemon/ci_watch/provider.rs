// ---------------------------------------------------------------------------
// Shared HTTP client for CI providers (Sprint 39 follow-up extraction)
// ---------------------------------------------------------------------------

/// Page size used by every provider's `poll_runs` query (GitHub
/// `per_page`, GitLab `per_page`, Bitbucket `pagelen`). Caps how many of
/// the most-recent runs on a branch we examine per poll.
///
/// The aggregator (`poller::aggregate_conclusion_for_sha`) groups runs
/// by `head_sha` — only runs matching the current head contribute, the
/// rest are filtered out, so a larger page costs ~one extra parse per
/// stale entry and a few KB of bytes; rate-limit cost is unchanged (one
/// call per poll regardless of page size).
///
/// Pre-bump value was 5. A push that fans out to ≥5 workflows would
/// drop the oldest run from the response window, and the aggregator
/// would silently report `success` from the surviving subset even if
/// the dropped run failed. 20 gives 4× headroom over the present
/// agend-terminal push fan-out and stays well under GitHub's 100-cap.
pub(crate) const POLL_RUNS_PAGE_SIZE: u32 = 20;

/// Auth scheme for CI provider HTTP requests.
pub(crate) enum CiAuth {
    /// `Authorization: Bearer <token>` (GitHub)
    Bearer(String),
    /// Custom header name + value (GitLab: `PRIVATE-TOKEN: <token>`)
    Header(String, String),
    /// HTTP Basic auth `user:password` (Bitbucket)
    Basic(String, String),
}

/// Shared HTTP client wrapping reqwest + auth + base URL.
/// Each CiProvider stores one and delegates request construction.
pub(crate) struct CiHttpClient {
    client: reqwest::Client,
    /// pub(super) so the #701-split test mod (now in `super::poller`) can
    /// assert constructor URL routing.
    pub(super) base_url: String,
    /// Path prefix inserted between base_url and the caller's path
    /// (e.g., "/api/v4" for GitLab, "/2.0" for Bitbucket, "" for GitHub).
    path_prefix: String,
    /// Auth resolver called per-request (token may change at runtime).
    auth_fn: Box<dyn Fn() -> Option<CiAuth> + Send + Sync>,
    /// Per-provider Accept header (e.g., GitHub's `application/vnd.github+json`).
    default_accept: Option<String>,
}

impl CiHttpClient {
    pub(crate) fn new(
        base_url: String,
        path_prefix: &str,
        auth_fn: impl Fn() -> Option<CiAuth> + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        Self::with_accept(base_url, path_prefix, None, auth_fn)
    }

    pub(crate) fn with_accept(
        base_url: String,
        path_prefix: &str,
        default_accept: Option<String>,
        auth_fn: impl Fn() -> Option<CiAuth> + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
            base_url,
            path_prefix: path_prefix.to_string(),
            auth_fn: Box::new(auth_fn),
            default_accept,
        })
    }

    /// Build a GET request with auth + User-Agent applied.
    pub(crate) fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = if self.path_prefix.is_empty() {
            format!("{}/{path}", self.base_url)
        } else {
            format!("{}/{}/{path}", self.base_url, self.path_prefix)
        };
        let mut req = self.client.get(&url).header("User-Agent", "agend-terminal");
        if let Some(ref accept) = self.default_accept {
            req = req.header("Accept", accept.as_str());
        }
        if let Some(auth) = (self.auth_fn)() {
            req = match auth {
                CiAuth::Bearer(token) => req.bearer_auth(token),
                CiAuth::Header(name, value) => req.header(name, value),
                CiAuth::Basic(user, pass) => req.basic_auth(user, Some(pass)),
            };
        }
        req
    }

    /// Parse rate-limit reset timestamp from response headers.
    /// Checks both GitHub (`x-ratelimit-reset`) and GitLab (`ratelimit-reset`) header names.
    #[allow(dead_code)] // available for providers to use; wired per-provider as needed
    pub(crate) fn parse_rate_limit_reset(headers: &reqwest::header::HeaderMap) -> Option<u64> {
        headers
            .get("x-ratelimit-reset")
            .or_else(|| headers.get("ratelimit-reset"))
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }
}

// ---------------------------------------------------------------------------
// CiProvider trait — abstracts CI-server-specific HTTP calls so ci_watch
// state-machine logic can be tested with a mock and future providers
// (GitLab, Buildkite, …) can be added without touching the orchestration.
// ---------------------------------------------------------------------------

/// A single CI pipeline/workflow run, provider-neutral.
#[derive(Debug, Clone)]
pub struct CiRun {
    pub id: u64,
    /// `None` means in-progress / not yet concluded.
    pub conclusion: Option<String>,
    pub head_sha: String,
    pub url: String,
    /// Workflow name (e.g. "CI", "LOC Overrun Check").
    /// Used by #1151 to filter to required checks only.
    pub name: String,
    /// #1859 Fix B: GitHub Actions `run_attempt` (1 for the first run, +1 per
    /// `gh run rerun`). A rerun keeps the SAME `id` (+ head_sha + conclusion) and
    /// only bumps this, so the dedup gates treat an attempt INCREASE as a new
    /// notifiable event (otherwise a flake-rerun fail→pass is silently swallowed).
    /// Providers without an attempt concept (a retry mints a new run id) report 1.
    pub run_attempt: u64,
}

/// A single job within a CI run (#1326 job-level early-fail).
#[derive(Debug, Clone)]
pub struct CiJob {
    pub name: String,
    pub conclusion: Option<String>,
}

/// Result of polling CI runs for a branch.
///
/// Sprint 54 P0-2: the success variant carries the
/// `X-RateLimit-Remaining` / `X-RateLimit-Limit` quota counters when
/// the provider exposes them. The tick-loop feeds these into
/// [`crate::daemon::ci_watch::adaptive_interval`] to widen the next poll's effective interval
/// before the limit is exhausted (preempt vs. recover).
#[derive(Debug, Clone)]
pub enum CiPollResult {
    /// Runs retrieved successfully (may be empty). Rate-limit fields
    /// are `None` for providers that don't expose the quota headers
    /// (currently only GitHub does); callers fall back to the
    /// configured interval in that case.
    Runs {
        runs: Vec<CiRun>,
        /// Last seen `X-RateLimit-Remaining`. None if header absent.
        rate_limit_remaining: Option<u64>,
        /// Last seen `X-RateLimit-Limit`. None if header absent.
        rate_limit_limit: Option<u64>,
    },
    /// API-level error (rate limit, auth failure, server error).
    ApiError {
        #[allow(dead_code)] // serialized in error diagnostics
        status: u16,
        message: String,
        /// If rate-limited, epoch seconds when quota resets.
        rate_limit_reset: Option<u64>,
    },
}

/// #1705: one run row from a REPO-LEVEL batch poll — a `CiRun` tagged with its
/// `head_branch`, so a single repo query can be fanned out to each watched branch.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub head_branch: String,
    pub run: CiRun,
}

/// #1705: result of ONE repo-level batch poll (`actions/runs?per_page=100`, no
/// `?branch=` filter) — replaces N per-branch polls with one. Rate-limit fields
/// are repo-level (a single response).
#[derive(Debug, Clone)]
pub enum RepoPollResult {
    Runs {
        rows: Vec<RunRow>,
        rate_limit_remaining: Option<u64>,
        rate_limit_limit: Option<u64>,
    },
    ApiError {
        #[allow(dead_code)] // serialized in error diagnostics
        status: u16,
        message: String,
        rate_limit_reset: Option<u64>,
    },
}

/// PR terminal-state check result.
#[derive(Debug)]
pub enum PrState {
    /// PR reached terminal state (closed or merged).
    Terminal { merged: bool },
    /// PR is still open.
    Open,
    /// Check failed or no PR found — leave watcher alone.
    Unknown,
}

/// #813: PR mergeable-state result. GitHub returns `mergeable_state`
/// as one of: clean / dirty / unstable / blocked / behind / unknown.
/// We collapse to the operator-actionable subset:
/// - `Mergeable` — clean / unstable (CI failures are a separate signal)
/// - `Conflicting` — dirty (merge conflict with base)
/// - `Unstable` — blocked / behind (review-policy / branch-behind, not
///   a conflict per se but worth surfacing)
/// - `Unknown` — query failed, fail-open path (no alert, no block)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unstable,
    Unknown,
}

impl MergeableState {
    /// Stable string representation written to watch JSON + status
    /// response. Watch JSON is read back as `&str`; no `from_str`
    /// helper is needed because the only consumer compares against
    /// the literal `"CONFLICTING"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mergeable => "MERGEABLE",
            Self::Conflicting => "CONFLICTING",
            Self::Unstable => "UNSTABLE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Abstraction over a CI server's REST API.
/// Each method corresponds to one provider-specific HTTP call.
/// Return types are provider-neutral — all schema parsing happens
/// inside the impl, not in the ci_watch state machine.
#[async_trait::async_trait]
pub trait CiProvider: Send + Sync {
    /// Poll workflow/pipeline runs for `repo@branch`.
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult>;

    /// #1705: poll ALL recent runs for `repo` in ONE query (`?per_page=100`, no
    /// branch filter); the caller groups by `head_branch` and fans out to each
    /// watched branch, collapsing N per-branch polls into one. `None` = the
    /// provider does not support batch polling → the caller falls back to
    /// per-branch [`poll_runs`] (GitLab / Bitbucket — unverified path).
    async fn poll_repo_runs(&self, _repo: &str) -> Option<anyhow::Result<RepoPollResult>> {
        None
    }

    /// Check whether the PR/MR for `branch` has reached a terminal state.
    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState;

    /// #813: Check the PR/MR's mergeable state. Returns `Unknown` on
    /// any query failure (rate-limit, auth, network, no PR found) so
    /// callers fail-open without alerting.
    ///
    /// Cross-backend stance §3.7: GitHub provider does the real query;
    /// GitLab / Bitbucket return `Unknown` (unverified — no operator
    /// has exercised the path) and the caller's fail-open guard
    /// suppresses the alert. Promotion blocked behind a fleet
    /// running on that backend.
    async fn check_pr_mergeable(&self, repo: &str, branch: &str) -> MergeableState {
        let _ = (repo, branch);
        MergeableState::Unknown
    }

    /// #813: synchronous variant for non-async callers (handler-layer
    /// MCP entry points that need a blocking answer on watch-start).
    /// Runs the async future on the shared CI runtime in a scoped
    /// thread — works regardless of whether the caller is already
    /// inside a tokio runtime.
    fn check_pr_mergeable_blocking(&self, repo: &str, branch: &str) -> MergeableState {
        std::thread::scope(|s| {
            let handle = s.spawn(|| {
                super::poller::shared_ci_runtime().block_on(self.check_pr_mergeable(repo, branch))
            });
            handle.join().unwrap_or(MergeableState::Unknown)
        })
    }

    /// Fetch a human-readable summary of the first failed job/step.
    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String;

    /// #1326: fetch jobs for a run to detect early job-level failures.
    /// Default returns empty — only GitHub implements this currently.
    async fn fetch_run_jobs(&self, _repo: &str, _run_id: u64) -> Vec<CiJob> {
        Vec::new()
    }

    /// CI-fail-notify: fetch the tail of the failed-job logs (~`max_lines`) so
    /// the daemon can inject the actual error inline instead of telling the
    /// agent to run `gh run view --log-failed` itself. Default `None` → callers
    /// fall back to the instruction-only body (so mocks / non-GitHub providers
    /// are unaffected). MUST stay async — the GitHub impl shells out via
    /// `tokio::process` and never blocks the shared ci runtime (#1476).
    async fn fetch_failure_log_tail(
        &self,
        _repo: &str,
        _run_id: u64,
        _max_lines: usize,
    ) -> Option<String> {
        None
    }

    /// Optional token/auth warning shown in the `watch_ci` MCP response.
    /// Currently called via `github_token_warning_from_env()` in the handler;
    /// future providers will use this method directly.
    #[allow(dead_code)] // trait method; future providers use directly
    fn token_warning(&self) -> Option<&'static str>;
}

/// GitHub Actions implementation of [`CiProvider`].
pub struct GitHubCiProvider {
    pub(super) http: CiHttpClient,
}

impl GitHubCiProvider {
    #[allow(dead_code)] // used by tests + future direct callers
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url("https://api.github.com".to_string())
    }

    pub fn with_base_url(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: CiHttpClient::with_accept(
                base_url,
                "",
                Some("application/vnd.github+json".to_string()),
                // Sprint 54 P0-4: token resolution now goes through the
                // centralized cache (env → gh CLI → None). The cache
                // discovers once per process and never writes back to env,
                // so child PTYs don't silently inherit a token.
                || crate::github_token::cached_token().map(CiAuth::Bearer),
            )?,
        })
    }
}

#[async_trait::async_trait]
impl CiProvider for GitHubCiProvider {
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult> {
        let resp = self
            .http
            .get(&format!(
                "repos/{repo}/actions/runs?branch={branch}&per_page={POLL_RUNS_PAGE_SIZE}"
            ))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let rate_limit_reset = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        // Sprint 54 P0-2: capture remaining/limit on every response,
        // not just rate-limited ones. The watch loop feeds these into
        // `adaptive_interval` so we widen the next poll BEFORE hitting
        // the cap, instead of recovering from it.
        let parse_u64_header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        };
        let rate_limit_remaining = parse_u64_header("x-ratelimit-remaining");
        let rate_limit_limit = parse_u64_header("x-ratelimit-limit");
        let body: serde_json::Value = resp.json().await?;

        // Surface API errors (rate-limit, auth, server) instead of
        // silently treating them as "no runs".
        if !(200..300).contains(&status) {
            let message = body["message"]
                .as_str()
                .unwrap_or("(no message)")
                .to_string();
            // Sprint 54 P0-4: hint via the unified token cache. Anything
            // the cache treats as "no token available" (env unset AND gh
            // not authed) gets the actionable hint. Reading the cache —
            // not env — keeps behavior consistent with what auth_fn
            // actually saw on the wire.
            let hint = if status == 403
                && crate::github_token::cached_token().is_none()
                && message.to_lowercase().contains("rate limit")
            {
                " — set GITHUB_TOKEN or run `gh auth login` to raise the unauthenticated 60/hr cap"
            } else {
                ""
            };
            return Ok(CiPollResult::ApiError {
                status,
                message: format!("GH API {status}: {message}{hint}"),
                rate_limit_reset,
            });
        }

        // Parse GitHub-specific `workflow_runs` array into neutral CiRun structs.
        let runs = body["workflow_runs"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(CiRun {
                            id: r["id"].as_u64()?,
                            conclusion: r["conclusion"].as_str().map(String::from),
                            head_sha: r["head_sha"].as_str()?.to_string(),
                            url: r["html_url"].as_str().unwrap_or("").to_string(),
                            name: r["name"].as_str().unwrap_or("").to_string(),
                            run_attempt: r["run_attempt"].as_u64().unwrap_or(1),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(CiPollResult::Runs {
            runs,
            rate_limit_remaining,
            rate_limit_limit,
        })
    }

    async fn poll_repo_runs(&self, repo: &str) -> Option<anyhow::Result<RepoPollResult>> {
        // #1705: ONE repo-level query (no `?branch=`) → all recent runs across
        // branches; the caller groups by `head_branch`. `per_page=100` (GitHub max)
        // covers every active watched branch's latest run in one page (active =
        // recent run = in-page; terminal watches are auto-cleared and don't poll).
        Some(async {
            let resp = self
                .http
                .get(&format!("repos/{repo}/actions/runs?per_page=100"))
                .send()
                .await?;
            let status = resp.status().as_u16();
            let rate_limit_reset = resp
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let parse_u64_header = |name: &str| {
                resp.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
            };
            let rate_limit_remaining = parse_u64_header("x-ratelimit-remaining");
            let rate_limit_limit = parse_u64_header("x-ratelimit-limit");
            let body: serde_json::Value = resp.json().await?;
            if !(200..300).contains(&status) {
                let message = body["message"].as_str().unwrap_or("(no message)").to_string();
                let hint = if status == 403
                    && crate::github_token::cached_token().is_none()
                    && message.to_lowercase().contains("rate limit")
                {
                    " — set GITHUB_TOKEN or run `gh auth login` to raise the unauthenticated 60/hr cap"
                } else {
                    ""
                };
                return Ok(RepoPollResult::ApiError {
                    status,
                    message: format!("GH API {status}: {message}{hint}"),
                    rate_limit_reset,
                });
            }
            // Same per-run parse as `poll_runs`, plus `head_branch` for grouping.
            let rows = body["workflow_runs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| {
                            Some(RunRow {
                                head_branch: r["head_branch"].as_str()?.to_string(),
                                run: CiRun {
                                    id: r["id"].as_u64()?,
                                    conclusion: r["conclusion"].as_str().map(String::from),
                                    head_sha: r["head_sha"].as_str()?.to_string(),
                                    url: r["html_url"].as_str().unwrap_or("").to_string(),
                                    name: r["name"].as_str().unwrap_or("").to_string(),
                                    run_attempt: r["run_attempt"].as_u64().unwrap_or(1),
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(RepoPollResult::Runs {
                rows,
                rate_limit_remaining,
                rate_limit_limit,
            })
        }
        .await)
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        // Sprint 54 Hotfix F gap fix: GitHub's `head=` query parameter
        // requires `user:ref-name` format. Sending bare `head=feat/foo`
        // makes the API silently drop the filter and return the most
        // recently created PR in the repo regardless of branch — that
        // behavior masked Hotfix F's freshness check (the misrouted PR
        // was usually fresh enough to pass the 1-hour window) and
        // produced false `Terminal{merged}` for any branch that had
        // never had a PR opened. Per GitHub docs:
        // https://docs.github.com/en/rest/pulls/pulls#list-pull-requests
        let owner = repo.split('/').next().unwrap_or("");
        let resp: serde_json::Value = match self
            .http
            .get(&format!(
                "repos/{repo}/pulls?head={owner}:{branch}&state=all&per_page=1"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return PrState::Unknown,
            },
            Err(_) => return PrState::Unknown,
        };
        match resp.as_array().and_then(|a| a.first()) {
            Some(pr) => {
                // Sprint 54 Hotfix F gap fix (defensive): even with the
                // correct query format, GitHub may return a PR whose
                // `head.ref` doesn't match what we asked for (cross-fork
                // edge cases, schema drift, future API quirks). Treat
                // mismatch as Unknown rather than trusting the response
                // — the cost of an extra polling tick is far smaller
                // than a false auto-clear.
                if pr["head"]["ref"].as_str() != Some(branch) {
                    tracing::debug!(
                        repo,
                        branch,
                        returned_ref = ?pr["head"]["ref"].as_str(),
                        "check_pr_terminal: response head.ref mismatch — returning Unknown"
                    );
                    return PrState::Unknown;
                }
                match pr["state"].as_str() {
                    Some("closed") => {
                        // Verify this PR was updated recently (not a stale PR from
                        // a previous use of the same branch name). If closed_at is
                        // older than 1 hour, treat as stale → Unknown (pending).
                        if let Some(closed_at) = pr["closed_at"].as_str() {
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(closed_at) {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(dt.with_timezone(&chrono::Utc));
                                if age > chrono::Duration::hours(1) {
                                    // Stale PR from previous branch use — not terminal.
                                    return PrState::Unknown;
                                }
                            }
                        }
                        PrState::Terminal {
                            merged: pr["merged_at"].as_str().is_some(),
                        }
                    }
                    Some(_) => PrState::Open,
                    None => PrState::Unknown,
                }
            }
            None => PrState::Unknown,
        }
    }

    /// #813: Query the PR's `mergeable_state` field. Requires two
    /// GETs because the list endpoint doesn't compute `mergeable` —
    /// only the per-PR detail endpoint does. Per GitHub docs:
    /// https://docs.github.com/en/rest/pulls/pulls#get-a-pull-request
    ///
    /// Fail-open on any error (network, auth, parse, missing) so the
    /// caller's guard suppresses the alert under transient issues.
    async fn check_pr_mergeable(&self, repo: &str, branch: &str) -> MergeableState {
        let owner = repo.split('/').next().unwrap_or("");
        // (1) List endpoint to resolve PR number from branch.
        let list: serde_json::Value = match self
            .http
            .get(&format!(
                "repos/{repo}/pulls?head={owner}:{branch}&state=open&per_page=1"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return MergeableState::Unknown,
            },
            Err(_) => return MergeableState::Unknown,
        };
        let pr_number = match list
            .as_array()
            .and_then(|a| a.first())
            .and_then(|pr| pr["number"].as_u64())
        {
            Some(n) => n,
            None => return MergeableState::Unknown,
        };
        // (2) Detail endpoint reads `mergeable_state` (computed
        // asynchronously by GitHub post-push — value may be "unknown"
        // for ~seconds after a push, which we surface as Unknown).
        let detail: serde_json::Value = match self
            .http
            .get(&format!("repos/{repo}/pulls/{pr_number}"))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return MergeableState::Unknown,
            },
            Err(_) => return MergeableState::Unknown,
        };
        match detail["mergeable_state"].as_str() {
            Some("dirty") => MergeableState::Conflicting,
            Some("clean") => MergeableState::Mergeable,
            Some("blocked") | Some("behind") => MergeableState::Unstable,
            Some("unstable") => MergeableState::Mergeable,
            _ => MergeableState::Unknown,
        }
    }

    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String {
        let jobs_resp: serde_json::Value = match self
            .http
            .get(&format!("repos/{repo}/actions/runs/{run_id}/jobs"))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return "unknown step".to_string(),
            },
            Err(_) => return "unknown step".to_string(),
        };
        jobs_resp["jobs"]
            .as_array()
            .and_then(|jobs| {
                jobs.iter().find_map(|job| {
                    job["steps"].as_array().and_then(|steps| {
                        steps.iter().find_map(|step| {
                            (step["conclusion"].as_str() == Some("failure")).then(|| {
                                format!(
                                    "{} / {}",
                                    job["name"].as_str().unwrap_or("?"),
                                    step["name"].as_str().unwrap_or("?")
                                )
                            })
                        })
                    })
                })
            })
            .unwrap_or_else(|| "unknown step".to_string())
    }

    async fn fetch_run_jobs(&self, repo: &str, run_id: u64) -> Vec<CiJob> {
        let resp = match self
            .http
            .get(&format!("repos/{repo}/actions/runs/{run_id}/jobs"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        body["jobs"]
            .as_array()
            .map(|jobs| {
                jobs.iter()
                    .filter_map(|j| {
                        Some(CiJob {
                            name: j["name"].as_str()?.to_string(),
                            conclusion: j["conclusion"].as_str().map(String::from),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn fetch_failure_log_tail(
        &self,
        repo: &str,
        run_id: u64,
        max_lines: usize,
    ) -> Option<String> {
        // `gh run view <id> --log-failed -R <repo>` flattens the failed-job logs
        // to text. Run the (blocking) CLI on the blocking pool so the async ci
        // runtime stays free — and so we NEVER `block_on` the shared ci runtime
        // (#1476: that would panic "cannot start a runtime from within a
        // runtime"). Bounded by a timeout so a wedged `gh` can't stall the poll.
        let repo_log = repo.to_string(); // for diagnostic warns (repo is moved into the closure)
        let repo = repo.to_string();
        let run = tokio::task::spawn_blocking(move || {
            std::process::Command::new("gh")
                .args([
                    "run",
                    "view",
                    &run_id.to_string(),
                    "--log-failed",
                    "-R",
                    &repo,
                ])
                .output()
        });
        // #1537 follow-up: log WHY a fetch yields no tail instead of swallowing
        // it (every failure mode used to collapse to a silent `None`, so a
        // tail-less notification was undiagnosable — e.g. the #1542 windows
        // case). Still always returns None on any failure (notification degrades
        // to the no-tail checklist); the warn is observability only.
        let out = match tokio::time::timeout(std::time::Duration::from_secs(20), run).await {
            Err(_) => {
                tracing::warn!(repo = %repo_log, run_id, "#1537: failed-log tail fetch timed out (20s) — notification omits tail");
                return None;
            }
            Ok(Err(join_err)) => {
                tracing::warn!(repo = %repo_log, run_id, error = %join_err, "#1537: failed-log tail fetch task join error");
                return None;
            }
            Ok(Ok(Err(exec_err))) => {
                tracing::warn!(repo = %repo_log, run_id, error = %exec_err, "#1537: failed-log tail fetch could not exec `gh`");
                return None;
            }
            Ok(Ok(Ok(out))) => out,
        };
        if !out.status.success() {
            let stderr: String = String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(300)
                .collect();
            tracing::warn!(repo = %repo_log, run_id, code = ?out.status.code(), stderr = %stderr, "#1537: `gh run view --log-failed` non-zero (run may be incomplete) — notification omits tail");
            return None;
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            tracing::warn!(repo = %repo_log, run_id, "#1537: `gh run view --log-failed` empty stdout — notification omits tail");
            return None;
        }
        // 8 KiB byte cap alongside the line cap (defends against one huge line).
        Some(super::poller::format_log_tail(
            trimmed, max_lines, 8192, run_id,
        ))
    }

    fn token_warning(&self) -> Option<&'static str> {
        github_token_warning(std::env::var("GITHUB_TOKEN").ok().as_deref())
    }
}

// ---------------------------------------------------------------------------
// GitLab CI provider
// ---------------------------------------------------------------------------

/// GitLab Pipelines implementation of [`CiProvider`].
pub struct GitLabCiProvider {
    pub(super) http: CiHttpClient,
}

impl GitLabCiProvider {
    #[allow(dead_code)] // wired in Sprint 39 PR-3 (fleet.yaml ci_provider config)
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url("https://gitlab.com".to_string())
    }

    pub fn with_base_url(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: CiHttpClient::new(base_url, "api/v4", || {
                Self::resolve_token().map(|t| CiAuth::Header("PRIVATE-TOKEN".into(), t))
            })?,
        })
    }

    /// Resolve GitLab auth token via fallback chain:
    /// 1. GITLAB_TOKEN env var
    /// 2. glab CLI config (~/.config/glab-cli/config.yml)
    fn resolve_token() -> Option<String> {
        if let Ok(token) = std::env::var("GITLAB_TOKEN") {
            return Some(token);
        }
        // Fallback: glab CLI config file at $HOME/.config/glab-cli/config.yml.
        let home = std::env::var("HOME").ok()?;
        let config_path = std::path::PathBuf::from(home)
            .join(".config")
            .join("glab-cli")
            .join("config.yml");
        let content = std::fs::read_to_string(config_path).ok()?;
        // glab config stores token as `token: <value>` under hosts.
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(token) = trimmed.strip_prefix("token:") {
                let token = token.trim().to_string();
                if !token.is_empty() {
                    return Some(token);
                }
            }
        }
        None
    }

    /// URL-encode a `owner/repo` path for GitLab project ID.
    fn encode_project(repo: &str) -> String {
        repo.replace('/', "%2F")
    }
}

#[async_trait::async_trait]
impl CiProvider for GitLabCiProvider {
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult> {
        let project = Self::encode_project(repo);
        let resp = self
            .http
            .get(&format!(
                "projects/{project}/pipelines?ref={branch}&per_page={POLL_RUNS_PAGE_SIZE}"
            ))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let rate_limit_reset = resp
            .headers()
            .get("ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let body: serde_json::Value = resp.json().await?;

        if !(200..300).contains(&status) {
            let message = body["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("(no message)")
                .to_string();
            return Ok(CiPollResult::ApiError {
                status,
                message: format!("GitLab API {status}: {message}"),
                rate_limit_reset,
            });
        }

        let runs = body
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        let gl_status = r["status"].as_str()?;
                        let conclusion = match gl_status {
                            "success" => Some("success".to_string()),
                            "failed" => Some("failure".to_string()),
                            "canceled" => Some("cancelled".to_string()),
                            _ => None, // running/pending/etc → in-progress
                        };
                        Some(CiRun {
                            id: r["id"].as_u64()?,
                            conclusion,
                            head_sha: r["sha"].as_str()?.to_string(),
                            url: r["web_url"].as_str().unwrap_or("").to_string(),
                            name: r["name"].as_str().unwrap_or("").to_string(),
                            // GitLab retries mint a new pipeline id → no attempt concept.
                            run_attempt: 1,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Sprint 54 P0-2: GitLab uses different rate-limit headers
        // (`ratelimit-*` per CiHttpClient::parse_rate_limit_reset). Until
        // we add per-provider quota mapping, treat headers as absent
        // here — the throttle path falls through to the configured
        // baseline.
        Ok(CiPollResult::Runs {
            runs,
            rate_limit_remaining: None,
            rate_limit_limit: None,
        })
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        let project = Self::encode_project(repo);
        let resp: serde_json::Value = match self
            .http
            .get(&format!(
                "projects/{project}/merge_requests?source_branch={branch}&state=all&per_page=1"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return PrState::Unknown,
            },
            Err(_) => return PrState::Unknown,
        };
        match resp.as_array().and_then(|a| a.first()) {
            Some(mr) => match mr["state"].as_str() {
                Some("merged") => PrState::Terminal { merged: true },
                Some("closed") => PrState::Terminal { merged: false },
                Some("opened") => PrState::Open,
                _ => PrState::Unknown,
            },
            None => PrState::Unknown,
        }
    }

    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String {
        let project = Self::encode_project(repo);
        let jobs_resp: serde_json::Value = match self
            .http
            .get(&format!(
                "projects/{project}/pipelines/{run_id}/jobs?per_page=20"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return "unknown job".to_string(),
            },
            Err(_) => return "unknown job".to_string(),
        };
        let failed_job = jobs_resp.as_array().and_then(|jobs| {
            jobs.iter()
                .find(|job| job["status"].as_str() == Some("failed"))
        });
        let Some(job) = failed_job else {
            return "unknown job".to_string();
        };
        let stage = job["stage"].as_str().unwrap_or("?");
        let name = job["name"].as_str().unwrap_or("?");
        let header = format!("{stage} / {name}");

        // Chain: fetch job trace (log tail ~50 lines) for richer summary.
        let job_id = match job["id"].as_u64() {
            Some(id) => id,
            None => return header,
        };
        let trace = match self
            .http
            .get(&format!("projects/{project}/jobs/{job_id}/trace"))
            .send()
            .await
        {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => return header,
        };
        let tail: String = trace
            .lines()
            .rev()
            .take(50)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        if tail.is_empty() {
            header
        } else {
            format!("{header}\n---\n{tail}")
        }
    }

    fn token_warning(&self) -> Option<&'static str> {
        if Self::resolve_token().is_some() {
            None
        } else {
            Some("GITLAB_TOKEN not set and glab CLI config not found — API calls may be rate-limited or fail for private repos")
        }
    }
}

// ---------------------------------------------------------------------------
// Bitbucket Cloud CI provider
// ---------------------------------------------------------------------------

/// Bitbucket Cloud Pipelines implementation of [`CiProvider`].
/// Cloud-only MVP per Sprint 39 §11 #1; Bitbucket Server deferred to Sprint 41+.
pub struct BitbucketCiProvider {
    pub(super) http: CiHttpClient,
}

#[allow(dead_code)] // Constructors wired in Sprint 39 PR-3
impl BitbucketCiProvider {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url("https://api.bitbucket.org".to_string())
    }

    pub fn with_base_url(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: CiHttpClient::new(base_url, "2.0", || {
                Self::resolve_token().map(|t| {
                    let parts: Vec<&str> = t.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        CiAuth::Basic(parts[0].to_string(), parts[1].to_string())
                    } else {
                        CiAuth::Bearer(t)
                    }
                })
            })?,
        })
    }

    /// Resolve Bitbucket auth via fallback chain:
    /// 1. BITBUCKET_TOKEN env (format: "user:app_password")
    /// 2. ~/.config/bb/config (Bitbucket CLI config)
    fn resolve_token() -> Option<String> {
        if let Ok(token) = std::env::var("BITBUCKET_TOKEN") {
            return Some(token);
        }
        let home = std::env::var("HOME").ok()?;
        let config_path = std::path::PathBuf::from(home)
            .join(".config")
            .join("bb")
            .join("config");
        let content = std::fs::read_to_string(config_path).ok()?;
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(token) = trimmed.strip_prefix("token:") {
                let token = token.trim().to_string();
                if !token.is_empty() {
                    return Some(token);
                }
            }
        }
        None
    }
}

#[async_trait::async_trait]
impl CiProvider for BitbucketCiProvider {
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult> {
        let resp = self
            .http
            .get(&format!(
                "repositories/{repo}/pipelines/?target.branch={branch}&pagelen={POLL_RUNS_PAGE_SIZE}&sort=-created_on"
            ))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await?;

        if !(200..300).contains(&status) {
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("(no message)")
                .to_string();
            return Ok(CiPollResult::ApiError {
                status,
                message: format!("Bitbucket API {status}: {message}"),
                rate_limit_reset: None,
            });
        }

        let runs = body["values"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        let state = r["state"]["name"].as_str()?;
                        let conclusion = match state {
                            "COMPLETED" => {
                                let result = r["state"]["result"]["name"].as_str()?;
                                Some(match result {
                                    "SUCCESSFUL" => "success".to_string(),
                                    "FAILED" => "failure".to_string(),
                                    "STOPPED" => "cancelled".to_string(),
                                    other => other.to_lowercase(),
                                })
                            }
                            _ => None, // RUNNING/PENDING → in-progress
                        };
                        Some(CiRun {
                            id: r["build_number"].as_u64()?,
                            conclusion,
                            head_sha: r["target"]["commit"]["hash"].as_str()?.to_string(),
                            url: r["links"]["html"]["href"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                            name: r["pipeline"]["title"].as_str().unwrap_or("").to_string(),
                            // Bitbucket retries mint a new build number → no attempt concept.
                            run_attempt: 1,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Sprint 54 P0-2: Bitbucket Cloud doesn't expose GitHub-style
        // quota headers. Treat as absent — adaptive_interval falls
        // through to the configured baseline for non-GitHub providers.
        Ok(CiPollResult::Runs {
            runs,
            rate_limit_remaining: None,
            rate_limit_limit: None,
        })
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        let resp: serde_json::Value = match self
            .http
            .get(&format!(
                "repositories/{repo}/pullrequests?q=source.branch.name=\"{branch}\"&pagelen=1"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return PrState::Unknown,
            },
            Err(_) => return PrState::Unknown,
        };
        match resp["values"].as_array().and_then(|a| a.first()) {
            Some(pr) => match pr["state"].as_str() {
                Some("MERGED") => PrState::Terminal { merged: true },
                Some("DECLINED") | Some("SUPERSEDED") => PrState::Terminal { merged: false },
                Some("OPEN") => PrState::Open,
                _ => PrState::Unknown,
            },
            None => PrState::Unknown,
        }
    }

    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String {
        // Bitbucket uses pipeline UUID for steps, but we store build_number.
        // Steps endpoint: GET /repositories/{repo}/pipelines/{pipeline_uuid}/steps/
        // Since we have build_number, use it as pipeline selector.
        let steps_resp: serde_json::Value = match self
            .http
            .get(&format!(
                "repositories/{repo}/pipelines/{run_id}/steps/?pagelen=20"
            ))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => return "unknown step".to_string(),
            },
            Err(_) => return "unknown step".to_string(),
        };
        let failed_step = steps_resp["values"].as_array().and_then(|steps| {
            steps
                .iter()
                .find(|step| step["state"]["result"]["name"].as_str() == Some("FAILED"))
        });
        let Some(step) = failed_step else {
            return "unknown step".to_string();
        };
        let name = step["name"].as_str().unwrap_or("?").to_string();
        // Chain: fetch step log tail (~50 lines).
        let step_uuid = match step["uuid"].as_str() {
            Some(u) => u,
            None => return name,
        };
        let log = match self
            .http
            .get(&format!(
                "repositories/{repo}/pipelines/{run_id}/steps/{step_uuid}/log"
            ))
            .send()
            .await
        {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => return name,
        };
        let tail: String = log
            .lines()
            .rev()
            .take(50)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        if tail.is_empty() {
            name
        } else {
            format!("{name}\n---\n{tail}")
        }
    }

    fn token_warning(&self) -> Option<&'static str> {
        if Self::resolve_token().is_some() {
            None
        } else {
            Some("BITBUCKET_TOKEN not set and bb CLI config not found — API calls may fail for private repos")
        }
    }
}

/// Preventive warning shown in the `watch_ci` MCP response when no
/// GitHub token is available from any source.
///
/// Sprint 54 P0-4: this helper now delegates to
/// `crate::github_token::cached_setup_warning()` so the wording is in
/// one place. The argument is unused in production (kept for backward
/// API compatibility with existing unit tests that drive this with
/// synthetic input), but the global cache's verdict is what
/// `handle_watch_ci` actually surfaces. When env is set OR `gh` is
/// authed, no warning fires.
pub fn github_token_warning(token: Option<&str>) -> Option<&'static str> {
    // Pure form retained for the existing in-file unit tests:
    // "Some(non-blank) ⇒ None, None ⇒ Some(SETUP_WARNING)".
    match token.map(str::trim).filter(|s| !s.is_empty()) {
        Some(_) => None,
        None => Some(crate::github_token::SETUP_WARNING),
    }
}

/// Production warning surface — reads through the unified token cache.
/// Used by `handle_watch_ci` to attach `setup_warning` to the MCP
/// response when no token is reachable. Same source-of-truth as the
/// auth path in [`GitHubCiProvider::with_base_url`], so the warning
/// fires iff the next HTTP request would actually go unauthenticated.
pub fn github_token_warning_from_env() -> Option<&'static str> {
    crate::github_token::cached_setup_warning()
}

/// Auto-detect CI provider from a `repo` string (typically from git remote URL).
/// Returns `(provider_kind, custom_host)`. `custom_host=true` means the domain
/// doesn't exactly match the canonical host — caller should warn.
pub fn detect_provider_from_remote(repo: &str) -> (&'static str, bool) {
    if repo.contains("gitlab.com") {
        ("gitlab", false)
    } else if repo.contains("gitlab") {
        ("gitlab", true) // self-hosted GitLab
    } else if repo.contains("bitbucket.org") {
        ("bitbucket_cloud", false)
    } else if repo.contains("bitbucket") {
        ("bitbucket_cloud", true) // custom Bitbucket domain
    } else if repo.contains("github.com") {
        ("github", false) // canonical github.com
    } else if is_short_form_repo(repo) {
        // #1188: short-form `owner/name` (no dots, no protocol) — GitHub default, not custom.
        ("github", false)
    } else {
        // GitHub Enterprise custom domain OR fully unknown → default github + warn
        ("github", true)
    }
}

/// #1188: Detect `owner/name` short-form repo strings (e.g. "suzuke/agend-terminal").
/// Pattern: exactly one `/`, no dots, no colons, no protocol prefix.
fn is_short_form_repo(repo: &str) -> bool {
    let parts: Vec<&str> = repo.split('/').collect();
    parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && !repo.contains('.')
        && !repo.contains(':')
}

#[cfg(test)]
mod review_repro_daemon_ci_pr;
