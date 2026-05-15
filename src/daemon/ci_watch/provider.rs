// ---------------------------------------------------------------------------
// Shared HTTP client for CI providers (Sprint 39 follow-up extraction)
// ---------------------------------------------------------------------------

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
}

/// Result of polling CI runs for a branch.
///
/// Sprint 54 P0-2: the success variant carries the
/// `X-RateLimit-Remaining` / `X-RateLimit-Limit` quota counters when
/// the provider exposes them. The tick-loop feeds these into
/// [`crate::daemon::ci_watch::adaptive_interval`] to widen the next poll's effective interval
/// before the limit is exhausted (preempt vs. recover).
#[derive(Debug)]
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
        #[allow(dead_code)]
        status: u16,
        message: String,
        /// If rate-limited, epoch seconds when quota resets.
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
    /// Always runs the async future on a fresh current-thread runtime
    /// in a scoped thread — works regardless of whether the caller is
    /// already inside a tokio runtime (avoids the multi-thread vs
    /// current-thread runtime-flavor branch). Dead-code allow lifts
    /// at C3 when handle_watch_ci wires the call site.
    fn check_pr_mergeable_blocking(&self, repo: &str, branch: &str) -> MergeableState {
        std::thread::scope(|s| {
            let handle = s.spawn(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return MergeableState::Unknown,
                };
                rt.block_on(self.check_pr_mergeable(repo, branch))
            });
            handle.join().unwrap_or(MergeableState::Unknown)
        })
    }

    /// Fetch a human-readable summary of the first failed job/step.
    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String;

    /// Optional token/auth warning shown in the `watch_ci` MCP response.
    /// Currently called via `github_token_warning_from_env()` in the handler;
    /// future providers will use this method directly.
    #[allow(dead_code)]
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

    #[allow(dead_code)] // used by auto-detect for GHE; wired in this PR
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
                "repos/{repo}/actions/runs?branch={branch}&per_page=5"
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

    #[allow(dead_code)]
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
                "projects/{project}/pipelines?ref={branch}&per_page=5"
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
                "repositories/{repo}/pipelines/?target.branch={branch}&pagelen=5&sort=-created_on"
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
    } else {
        // GitHub Enterprise custom domain OR fully unknown → default github + warn
        ("github", true)
    }
}
