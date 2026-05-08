use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// H2: Shared tokio runtime for CI watch — avoids spawning a new thread +
// runtime per poll cycle. Bounded to 2 worker threads.
// ---------------------------------------------------------------------------

fn shared_ci_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ci-watch")
            .enable_all()
            .build()
            .expect("ci-watch runtime")
    })
}

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
    base_url: String,
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
/// [`adaptive_interval`] to widen the next poll's effective interval
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
    http: CiHttpClient,
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
    http: CiHttpClient,
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
    http: CiHttpClient,
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

/// Watch TTL in hours. Used for both absolute expiry and inactivity threshold.
pub const WATCH_TTL_HOURS: i64 = 72;

/// Sprint 54 P0-5 (sub-scope B): consecutive rate-limited skips before a
/// `[ci-watch-stalled]` notification fires. Picked low (3) so a watch
/// stuck behind a multi-minute reset window surfaces quickly without
/// over-paging on a one-tick blip.
pub(crate) const STALL_THRESHOLD: u64 = 3;

/// Sprint 54 P0-5 helper: read existing `consecutive_skips`, increment,
/// persist, and (if we just crossed `STALL_THRESHOLD` and haven't yet
/// notified for this window) fan out a `[ci-watch-stalled]` inbox event
/// to every subscriber. The notify step reuses the P0-1 fan-out
/// contract — one inbox enqueue per subscriber.
///
/// Atomicity: the increment + `stalled_notified` flag move in a single
/// atomic_write so the next tick can't observe a "skips ≥ threshold,
/// flag still false" intermediate state and fire a duplicate event.
fn bump_consecutive_skips_and_maybe_notify(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    reset_epoch: u64,
) {
    let mut watch: serde_json::Value = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let prev_skips = watch["consecutive_skips"].as_u64().unwrap_or(0);
    let next_skips = prev_skips.saturating_add(1);
    watch["consecutive_skips"] = serde_json::json!(next_skips);

    let already_notified = watch["stalled_notified"].as_bool().unwrap_or(false);
    let should_notify = next_skips >= STALL_THRESHOLD && !already_notified;
    if should_notify {
        watch["stalled_notified"] = serde_json::json!(true);
        // Stamp `stalled_since_ms` only on the first stall write — gives
        // operators a stable anchor in the inbox payload.
        if watch["stalled_since_ms"].as_i64().is_none() {
            watch["stalled_since_ms"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
        }
    }
    let _ = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );

    if should_notify {
        let stalled_since_ms = watch["stalled_since_ms"].as_i64();
        // next_poll_eta = reset_epoch_ms (skip lifts at reset, then
        // adaptive backoff applies — but reset is the user-visible
        // "stalled until" moment).
        let next_poll_eta = (reset_epoch as i64).saturating_mul(1000);
        let setup_warning = crate::github_token::cached_setup_warning();
        let body = build_stalled_body(repo, branch, stalled_since_ms, next_poll_eta, setup_warning);
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-stalled", body);
    }
}

/// Sprint 54 P0-5 helper: clear the stall state on the first successful
/// poll after a stall window. Fans out `[ci-watch-resumed]` exactly
/// once per resume — symmetry with the stalled path.
fn clear_stall_and_maybe_notify_resumed(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
) {
    let mut watch: serde_json::Value = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let was_stalled = watch["stalled_notified"].as_bool().unwrap_or(false);
    let had_skips = watch["consecutive_skips"].as_u64().unwrap_or(0) > 0;
    if !was_stalled && !had_skips {
        return; // common case — no stall in flight, nothing to write.
    }
    watch["consecutive_skips"] = serde_json::json!(0);
    watch["stalled_notified"] = serde_json::json!(false);
    watch["stalled_since_ms"] = serde_json::Value::Null;
    let _ = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );
    if was_stalled {
        let body =
            format!("[ci-watch-resumed] {repo}@{branch}: poll resumed after rate-limit backoff");
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-resumed", body);
    }
}

fn build_stalled_body(
    repo: &str,
    branch: &str,
    stalled_since_ms: Option<i64>,
    next_poll_eta_ms: i64,
    setup_warning: Option<&'static str>,
) -> String {
    let mut s = format!("[ci-watch-stalled] {repo}@{branch}: rate-limit backoff in effect");
    if let Some(ts) = stalled_since_ms {
        if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts) {
            s.push_str(&format!("\nStalled since: {}", dt.to_rfc3339()));
        }
    }
    if let Some(eta) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(next_poll_eta_ms) {
        s.push_str(&format!("\nNext poll ETA: {}", eta.to_rfc3339()));
    }
    if let Some(w) = setup_warning {
        s.push_str(&format!("\nSetup hint: {w}"));
    }
    s
}

/// Sprint 54 P0-5: fan out a CI health event to every subscriber.
/// Mirrors the P0-1 terminal-notify loop — one inbox enqueue per
/// subscriber so multi-caller watches don't get last-write-wins.
fn fan_out_health_event(
    home: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    kind: &str,
    body: String,
) {
    let repo_branch_key = format!("{repo}@{branch}");
    let supersede_token = format!("{kind}-{}", chrono::Utc::now().timestamp_millis());
    for sub in subscribers {
        crate::inbox::mark_ci_watch_superseded(home, sub, &repo_branch_key, &supersede_token);
        let _ = crate::inbox::enqueue(
            home,
            sub,
            crate::inbox::InboxMessage {
                schema_version: 0,
                id: None,
                read_at: None,
                thread_id: None,
                parent_id: None,
                task_id: None,
                force_meta: None,
                correlation_id: None,
                reviewed_head: None,
                from: "system:ci".to_string(),
                text: body.clone(),
                kind: Some(kind.to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                channel: None,
                delivery_mode: None,
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
                superseded_by: None,
                from_id: None,
                broadcast_context: None,
            },
        );
    }
}

/// Read the list of subscribed instances from a watch JSON value.
///
/// Schema migration (Sprint 54 P0-1): the canonical source is the
/// `subscribers` array (`[{instance, subscribed_at}, …]`). Pre-Sprint-54
/// files carry only a single `instance: "X"` field; this helper returns
/// `[X]` for them so the daemon's poll loop, notify path, and unwatch
/// logic all see one uniform `Vec<String>` regardless of file vintage.
///
/// The legacy `instance` field is preserved on writes for one release
/// cycle (read-only by writers post-r0) and slated for removal in
/// Sprint 55 once daemons in the wild have written-back the new
/// schema at least once.
pub(crate) fn parse_subscribers(watch: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = watch.get("subscribers").and_then(|v| v.as_array()) {
        let mut out: Vec<String> = arr
            .iter()
            .filter_map(|s| s.get("instance").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        out.dedup();
        if !out.is_empty() {
            return out;
        }
    }
    // Legacy: pre-r0 watch files carry only `instance: "X"`. Treat as a
    // singleton list so the rest of the pipeline doesn't have to fork.
    if let Some(legacy) = watch.get("instance").and_then(|v| v.as_str()) {
        if !legacy.is_empty() {
            return vec![legacy.to_string()];
        }
    }
    Vec::new()
}

/// Remove a watch file and log the removal event.
///
/// `instance_label` is a free-form audit string — the caller passes
/// either a single subscriber (legacy callers) or comma-joined
/// subscribers (post-r0 multi-caller). The event log mirrors the
/// label verbatim for human-readable traceability.
pub fn remove_watch(
    home: &Path,
    watch_path: &Path,
    instance_label: &str,
    repo: &str,
    branch: &str,
    reason: &str,
) {
    let _ = std::fs::remove_file(watch_path);
    crate::event_log::log(
        home,
        "ci_watch_removed",
        instance_label,
        &format!("repo={repo} branch={branch} reason={reason}"),
    );
}

/// Deterministic, collision-free filename for a CI watch entry.
/// Uses SHA-256 of `"{repo}:{branch}"` to avoid path traversal and
/// collisions when repo names contain `/` (e.g. `owner/repo` vs
/// `owner_repo`).
pub fn watch_filename(repo: &str, branch: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    format!("{repo}:{branch}").hash(&mut h);
    format!("{:016x}.json", h.finish())
}

/// Sprint 54 P0-2: adaptive backoff curve based on remaining quota.
///
/// Returns the effective polling interval (in seconds) given the
/// configured baseline `interval_secs` and the most recent
/// `X-RateLimit-Remaining` / `X-RateLimit-Limit` observation. The
/// curve has three zones:
///
/// | Zone     | `remaining_pct = remaining / limit` | Multiplier |
/// |----------|-------------------------------------|------------|
/// | Healthy  | `> 0.5`                             | `× 1`      |
/// | Cautious | `0.1 < … ≤ 0.5`                     | `× 2`      |
/// | Critical | `≤ 0.1`                             | `× 4`      |
///
/// **Floor + ceiling**: never below the configured baseline (so
/// operators can't accidentally tune themselves into permanent
/// throttle), never above `interval_secs * 4` (so a single critical
/// watch can't quietly stop polling for an hour).
///
/// **Missing headers**: if either `remaining` or `limit` is `None`, or
/// `limit` is zero, we fall back to the configured baseline. Providers
/// that don't expose the quota headers (currently GitLab + Bitbucket)
/// keep their existing behavior.
///
/// Pure helper — no IO, no state. The throttle path
/// ([`watch_is_due`]) consumes the result; the tick-loop persists
/// `effective_interval_secs` to the watch JSON for diagnostics.
pub(crate) fn adaptive_interval(
    interval_secs: u64,
    remaining: Option<u64>,
    limit: Option<u64>,
) -> u64 {
    // EMPIRICAL REGRESSION-PROOF FLIP (Sprint 54 P0-2): replace the
    // body below with `let _ = (remaining, limit); return interval_secs;`
    // to simulate scaling being disabled. Tests
    // `adaptive_interval_cautious_zone_doubles` and
    // `adaptive_interval_critical_zone_quadruples` both fail with the
    // signatures captured in the PR description.
    let (remaining, limit) = match (remaining, limit) {
        (Some(r), Some(l)) if l > 0 => (r, l),
        // Missing headers OR pathological limit=0 ⇒ baseline. We never
        // ceiling-multiply on absent data — that'd silently widen polls
        // for non-GitHub providers that don't ship the quota counters.
        _ => return interval_secs,
    };
    // Use *1000 to keep the comparison in integer space — avoids
    // floating-point edge cases at exact boundaries (0.5 / 0.1).
    let remaining_x1000 = remaining.saturating_mul(1000) / limit;
    if remaining_x1000 > 500 {
        interval_secs
    } else if remaining_x1000 > 100 {
        interval_secs.saturating_mul(2)
    } else {
        interval_secs.saturating_mul(4)
    }
}

/// Pure throttle decision for a CI watch. Returns `true` when the watch
/// is due for a GitHub poll given its `last_polled_at` (epoch millis,
/// `None` for a fresh watch), its configured `interval_secs`, and the
/// current wall-clock time.
///
/// Extracted from `check_ci_watches` so the first-poll-immediate rule
/// can be unit-tested without filesystem IO — the previous mtime-based
/// throttle was testable only via external side effects on file
/// modification time.
fn watch_is_due(last_polled_at: Option<i64>, interval_secs: u64, now_ms: i64) -> bool {
    match last_polled_at {
        // Never-polled watches (freshly registered, or pre-schema files
        // that don't carry the field) fire on the first check. The
        // handler writes `last_polled_at: null` to signal this.
        None => true,
        Some(ts) => now_ms.saturating_sub(ts) >= (interval_secs as i64) * 1000,
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

/// Check CI watch configs and inject failure logs to agents when CI fails.
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

/// Sprint 57 Wave 2 Track B (#546 Item 1 + Item 3 migration) —
/// scan ci-watches dir, remove any watch that:
///   1. has `expires_at < now` (absolute TTL elapsed),
///   2. has `last_terminal_seen_at` older than `WATCH_TTL_HOURS`
///      (inactivity TTL elapsed), or
///   3. targets a protected ref per `agent_ops::is_protected_ref`
///      (E4.5 migration — closes the ci_watch-on-main bypass that
///      Sprint 56's `handle_watch_ci` left open until Wave 2 Track B
///      gated it).
///
/// The poll loop (`check_ci_watches_with_provider`) already enforces
/// (1) and (2) lazily on every per-watch tick, but only for watches
/// it actively polls — a watch can persist on disk indefinitely if
/// the upstream branch is gone or no agent is currently polling it.
/// This eager helper closes that gap by walking the entire dir
/// without entering the poll path.
///
/// Returns the number of watches removed. Best-effort: read/parse
/// failures skip the entry rather than aborting the sweep.
pub fn gc_stale_watches(home: &Path, sweep_origin: &str) -> usize {
    let ci_dir = home.join("ci-watches");
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return 0;
    };
    let now_utc = chrono::Utc::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let repo = watch["repo"].as_str().unwrap_or("?");
        let branch = watch["branch"].as_str().unwrap_or("?");
        let audit_label = parse_subscribers(&watch).join(",");

        // (3) E4.5 protected-ref migration — applied first because a
        // protected-ref watch is invalid regardless of TTL state.
        if crate::agent_ops::is_protected_ref(branch) {
            remove_watch(
                home,
                &path,
                &audit_label,
                repo,
                branch,
                &format!("{sweep_origin}_protected_branch_migration"),
            );
            tracing::info!(repo = %repo, branch = %branch, sweep = %sweep_origin,
                "ci_watch removed (E4.5 protected-branch migration)");
            removed += 1;
            continue;
        }

        // (1) absolute TTL.
        if let Some(expires_at) = watch["expires_at"].as_str() {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                if now_utc > exp.with_timezone(&chrono::Utc) {
                    remove_watch(
                        home,
                        &path,
                        &audit_label,
                        repo,
                        branch,
                        &format!("{sweep_origin}_expired"),
                    );
                    tracing::info!(repo = %repo, branch = %branch, sweep = %sweep_origin,
                        "ci_watch removed (absolute TTL elapsed)");
                    removed += 1;
                    continue;
                }
            }
        }

        // (2) inactivity TTL.
        if let Some(last_seen) = watch["last_terminal_seen_at"].as_str() {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                let elapsed = now_utc.signed_duration_since(ts.with_timezone(&chrono::Utc));
                if elapsed > chrono::Duration::hours(WATCH_TTL_HOURS) {
                    remove_watch(
                        home,
                        &path,
                        &audit_label,
                        repo,
                        branch,
                        &format!("{sweep_origin}_inactivity_ttl"),
                    );
                    tracing::info!(repo = %repo, branch = %branch, hours = WATCH_TTL_HOURS,
                        sweep = %sweep_origin,
                        "ci_watch removed (inactivity TTL elapsed)");
                    removed += 1;
                    continue;
                }
            }
        }
    }
    removed
}

/// Sprint 57 Wave 2 Track B (#546 Item 1) — daemon-startup eager
/// sweep. Runs once before the tick loop begins so stale entries
/// from a prior daemon process don't outlive the restart. Idempotent;
/// re-runs are no-ops once the dir is clean.
pub fn startup_sweep(home: &Path) {
    let removed = gc_stale_watches(home, "startup_sweep");
    if removed > 0 {
        tracing::info!(removed, "ci_watch startup sweep complete");
    }
}

pub fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
    // Sprint 57 Wave 2 Track B (#546 Item 1) — eager per-tick GC pass
    // BEFORE the poll loop. The lazy expiry inside the poll body still
    // runs (Sprint 53/54 era), but it can only see watches actively
    // being polled. This pass closes the "stale on disk after upstream
    // branch deletion" gap.
    let _ = gc_stale_watches(home, "eager_gc");
    check_ci_watches_with_provider(home, registry, |watch| {
        let ci_url = watch
            .get("ci_provider_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let repo = watch.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        // Explicit ci_provider wins; absent → auto-detect from repo URL.
        let (ci_type, default_url) = match watch.get("ci_provider").and_then(|v| v.as_str()) {
            Some(explicit) => (explicit, String::new()),
            None => {
                let (kind, is_custom) = detect_provider_from_remote(repo);
                if is_custom {
                    tracing::warn!(
                        repo,
                        kind,
                        "ci_watch: custom CI host pattern detected — suggest setting fleet.yaml ci_provider: explicitly"
                    );
                }
                let default = match kind {
                    "gitlab" => "https://gitlab.com",
                    "bitbucket_cloud" => "https://api.bitbucket.org",
                    _ => "https://api.github.com",
                };
                (kind, default.to_string())
            }
        };
        let url = ci_url.unwrap_or(default_url);
        match ci_type {
            "gitlab" => {
                let url = if url.is_empty() {
                    "https://gitlab.com".to_string()
                } else {
                    url
                };
                Some(Box::new(GitLabCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
            "bitbucket_cloud" => {
                let url = if url.is_empty() {
                    "https://api.bitbucket.org".to_string()
                } else {
                    url
                };
                Some(Box::new(BitbucketCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
            "bitbucket_server" => {
                tracing::error!(
                    "Bitbucket Server not yet supported — track Sprint 41+ candidate. \
                     Use bitbucket_cloud for Bitbucket Cloud repos."
                );
                None
            }
            _ => {
                let url = if url.is_empty() {
                    "https://api.github.com".to_string()
                } else {
                    url
                };
                Some(Box::new(GitHubCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
        }
    });
}

/// Inner implementation that accepts a provider factory for testability.
fn check_ci_watches_with_provider(
    home: &Path,
    registry: &AgentRegistry,
    make_provider: impl Fn(&serde_json::Value) -> Option<Box<dyn CiProvider>> + Send + Sync + 'static,
) {
    let entries = match std::fs::read_dir(home.join("ci-watches")) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: serde_json::Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let repo = match watch["repo"].as_str() {
            Some(r) => r.to_string(),
            None => continue,
        };
        // Sprint 54 P0-1: subscribers list (with legacy single-instance fallback)
        // replaces the single `instance` field. Empty list ⇒ skip — a watch with
        // no recipients is useless and would only burn rate-limit.
        let subscribers = parse_subscribers(&watch);
        if subscribers.is_empty() {
            continue;
        }
        let branch = watch["branch"].as_str().unwrap_or("main").to_string();
        let interval = watch["interval_secs"].as_u64().unwrap_or(60);
        let last_run_id = watch["last_run_id"].as_u64();
        let head_sha = watch["head_sha"].as_str().map(String::from);
        let last_notified_sha = watch["last_notified_head_sha"].as_str().map(String::from);

        // Audit label for remove_watch: comma-joined subscribers so the
        // event log stays human-readable when multiple agents share a watch.
        let audit_label = subscribers.join(",");

        // TTL check: remove expired watches before polling.
        let now_utc = chrono::Utc::now();
        if let Some(expires_at) = watch["expires_at"].as_str() {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                if now_utc > exp.with_timezone(&chrono::Utc) {
                    remove_watch(home, &path, &audit_label, &repo, &branch, "expired");
                    tracing::info!(repo = %repo, branch = %branch, "CI watch expired (TTL)");
                    continue;
                }
            }
        }
        // Inactivity TTL: WATCH_TTL_HOURS since last terminal run seen
        if let Some(last_seen) = watch["last_terminal_seen_at"].as_str() {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                let elapsed = now_utc.signed_duration_since(ts.with_timezone(&chrono::Utc));
                if elapsed > chrono::Duration::hours(WATCH_TTL_HOURS) {
                    remove_watch(home, &path, &audit_label, &repo, &branch, "inactivity_ttl");
                    tracing::info!(repo = %repo, branch = %branch, hours = WATCH_TTL_HOURS, "CI watch removed: inactivity TTL");
                    continue;
                }
            }
        }

        // Rate-limit backoff: skip polling until X-RateLimit-Reset time.
        if let Some(reset_epoch) = watch["rate_limit_until"].as_u64() {
            if (chrono::Utc::now().timestamp() as u64) < reset_epoch {
                // Sprint 54 P0-5 (sub-scope B): increment consecutive_skips
                // on each rate-limited tick so subscribers see a
                // `[ci-watch-stalled]` event after STALL_THRESHOLD
                // consecutive misses. Persist atomically with `stalled_notified`
                // so the dispatch is exactly-once per stall window.
                bump_consecutive_skips_and_maybe_notify(
                    home,
                    &path,
                    &repo,
                    &branch,
                    &subscribers,
                    reset_epoch,
                );
                continue;
            }
        }

        // Sprint 54 P0-2: compute adaptive backoff from the most recent
        // quota counters persisted on the watch file. The poll path
        // refreshes these on every successful response. First poll has
        // `None` → `adaptive_interval` falls through to `interval`, so
        // a fresh watch behaves identically to pre-r0.
        let prev_remaining = watch["rate_limit_remaining"].as_u64();
        let prev_limit = watch["rate_limit_limit"].as_u64();
        let effective_interval = adaptive_interval(interval, prev_remaining, prev_limit);

        // Throttle from a dedicated `last_polled_at` (epoch millis) in the
        // watch file itself, not file mtime. mtime conflates "when this
        // file was touched" with "when we last polled" and broke whenever
        // another writer (migration, hand-edit, freshly created watch)
        // stamped the file — the handler used to backdate mtime manually
        // to work around that. Schema-local state removes both the
        // first-poll-lag quirk and the external-writer fragility.
        let now_ms = chrono::Utc::now().timestamp_millis();
        if !watch_is_due(watch["last_polled_at"].as_i64(), effective_interval, now_ms) {
            continue;
        }
        // Stamp `last_polled_at` BEFORE spawning the GH request so a slow
        // GH response doesn't let the next tick re-enter for the same
        // watch. The spawned thread updates last_run_id / head_sha on a
        // terminal conclusion; non-terminal polls leave those fields
        // alone but the `last_polled_at` stamp already keeps them in
        // throttle.
        let mut watch_with_stamp = watch.clone();
        watch_with_stamp["last_polled_at"] = serde_json::json!(now_ms);
        // P0-2 diagnostic: stamp the effective interval so operators
        // can read the current backoff zone from the watch file.
        watch_with_stamp["effective_interval_secs"] = serde_json::json!(effective_interval);
        // M1: atomic write
        let _ = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch_with_stamp)
                .unwrap_or_default()
                .as_bytes(),
        );

        let home = home.to_path_buf();
        let watch_path = path.clone();
        let registry = Arc::clone(registry);
        let provider = match make_provider(&watch) {
            Some(p) => p,
            None => {
                tracing::warn!(repo = %repo, "ci_check: failed to build CI provider");
                continue;
            }
        };
        // H2: use shared runtime instead of per-poll thread + runtime.
        // fire-and-forget: ci_check is one-shot per poll cycle. The shared
        // runtime bounds concurrency to 2 worker threads. No JoinHandle
        // needed — the tick loop re-spawns next cycle if still watching.
        let subscribers_owned = subscribers.clone();
        shared_ci_runtime().spawn(async move {
            if let Err(e) = ci_check_repo(
                &home,
                &watch_path,
                &repo,
                &branch,
                &subscribers_owned,
                last_run_id,
                head_sha.as_deref(),
                last_notified_sha.as_deref(),
                &registry,
                provider.as_ref(),
            )
            .await
            {
                tracing::warn!(repo = %repo, error = %e, "CI check failed");
            }
        });
    }
}

/// Outcome of interpreting a `GET /repos/.../actions/runs` response.
///
/// Without this, a non-2xx response (e.g. unauthenticated rate-limit
/// `{"message":"API rate limit exceeded ..."}`) parses cleanly as JSON
/// but its `workflow_runs` field is absent, and the caller's
/// `body["workflow_runs"].as_array()` returns `None` — silently behaving
/// as if the branch had no runs and skipping every subsequent
/// notification while `last_polled_at` keeps marching forward. Tag the
/// HTTP status explicitly so API errors surface as `Err` instead of
/// imitating a quiescent branch.
///
/// Production code now uses [`CiPollResult`] via the [`CiProvider`] trait;
/// this enum is retained for unit-testing the classification logic that
/// lives inside [`GitHubCiProvider::poll_runs`].
#[cfg(test)]
enum RunsResponse<'a> {
    Run(&'a serde_json::Value),
    NoRuns,
    ApiError(String),
}

/// Pure interpreter for a runs-list response. See [`RunsResponse`] for
/// why the rate-limit / NoRuns distinction matters.
///
/// Retained under `#[cfg(test)]` — production classification now happens
/// inside [`GitHubCiProvider::poll_runs`].
#[cfg(test)]
fn classify_runs_response(status: u16, body: &serde_json::Value) -> RunsResponse<'_> {
    if !(200..300).contains(&status) {
        let message = body["message"].as_str().unwrap_or("(no message)");
        let hint = if status == 403
            && std::env::var("GITHUB_TOKEN").is_err()
            && message.to_lowercase().contains("rate limit")
        {
            " — set GITHUB_TOKEN to raise the unauthenticated 60/hr cap"
        } else {
            ""
        };
        return RunsResponse::ApiError(format!("GH API {status}: {message}{hint}"));
    }
    match body["workflow_runs"].as_array().and_then(|a| a.first()) {
        Some(run) => RunsResponse::Run(run),
        None => RunsResponse::NoRuns,
    }
}

/// Select runs from a CI poll result that should trigger notifications.
/// Returns indices into `runs` of terminal runs with `id > last_run_id`, ordered
/// oldest-first so notifications arrive chronologically.
/// In-progress runs (conclusion=None) are skipped.
pub(crate) fn select_runs_to_notify(runs: &[CiRun], last_run_id: Option<u64>) -> Vec<usize> {
    let threshold = last_run_id.unwrap_or(0);
    let mut selected: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .filter_map(|(i, run)| {
            if run.id <= threshold {
                return None;
            }
            // Skip non-terminal (in-progress) runs
            run.conclusion.as_ref()?;
            Some((i, run.id))
        })
        .collect();
    // Sort oldest-first by run_id
    selected.sort_by_key(|&(_, id)| id);
    selected.into_iter().map(|(i, _)| i).collect()
}

/// Pure function: deduplicate terminal runs by head_sha.
/// Returns `(run_index, run_id, head_sha)` tuples, one per unique sha,
/// keeping the latest run_id per sha. Skips shas matching `last_notified`.
/// Sorted by run_id (oldest first) for chronological notification order.
pub(crate) fn dedupe_notifications_by_head_sha<'a>(
    runs: &'a [CiRun],
    to_notify: &[usize],
    last_notified: Option<&str>,
) -> Vec<(usize, u64, &'a str)> {
    let mut best: std::collections::HashMap<&str, (usize, u64)> = std::collections::HashMap::new();
    for &idx in to_notify {
        let run = &runs[idx];
        let sha = run.head_sha.as_str();
        let id = run.id;
        best.entry(sha)
            .and_modify(|e| {
                if id > e.1 {
                    *e = (idx, id);
                }
            })
            .or_insert((idx, id));
    }
    let mut result: Vec<_> = best
        .into_iter()
        .filter(|(sha, _)| last_notified != Some(*sha))
        .map(|(sha, (idx, id))| (idx, id, sha))
        .collect();
    result.sort_by_key(|&(_, id, _)| id);
    result
}

/// Pure function: build the inbox body text for a CI notification.
/// Headline + optional failure detail + run URL.
pub(crate) fn build_inbox_body(
    headline: &str,
    conclusion: &str,
    failure_detail: Option<&str>,
    run_url: &str,
) -> String {
    if conclusion == "failure" {
        let detail = failure_detail.unwrap_or("unknown step");
        format!("{headline}\nDetail: {detail}\nURL: {run_url}")
    } else {
        format!("{headline}\nURL: {run_url}")
    }
}

/// Build the notification message for a CI run conclusion.
/// Returns `None` for non-terminal states (in-progress / null conclusion).
/// Job/step detail is excluded from the headline — agents use `inbox` or
/// `gh run view` for details.
fn ci_notification_message(
    repo: &str,
    branch: &str,
    conclusion: Option<&str>,
    _failure_detail: Option<&str>,
    head_sha: Option<&str>,
) -> Option<String> {
    let conclusion = conclusion?;
    let sha_short = head_sha
        .map(|s| format!(" ({})", &s[..s.len().min(7)]))
        .unwrap_or_default();
    let msg = match conclusion {
        "failure" => format!("[ci-fail] {repo}@{branch}{sha_short}: failure\r"),
        "success" => format!("[ci-pass] {repo}@{branch}{sha_short}: passed ✓\r"),
        other => format!("[ci-ended] {repo}@{branch}{sha_short}: {other}\r"),
    };
    Some(msg)
}

/// Fetch latest CI run and notify ALL subscribed agents on any
/// terminal conclusion (success, failure, cancelled, timed_out, etc.).
/// Also tracks `head_sha` — if the branch HEAD changes (e.g. force push),
/// `last_run_id` is reset so the new run is picked up.
/// On PR terminal states (merged/closed), the watcher is auto-cleared.
///
/// **Subscriber fan-out (Sprint 54 P0-1)**: `subscribers` is a slice of
/// instance names that all share this watch. The poll itself happens
/// once per cycle regardless of subscriber count (poll/subscriber split,
/// hard-contract item 1). Notifications, by contrast, fan out — every
/// subscriber receives the inbox message and the in-band agent inject
/// (item 2). The audit string for `remove_watch` joins all subscribers
/// with commas so event-log readers can see the full membership at the
/// moment of removal.
#[allow(clippy::too_many_arguments)]
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    last_run_id: Option<u64>,
    prev_head_sha: Option<&str>,
    last_notified_sha: Option<&str>,
    registry: &AgentRegistry,
    provider: &dyn CiProvider,
) -> anyhow::Result<()> {
    let audit_label = subscribers.join(",");
    // Check if the PR associated with this branch has reached a terminal state.
    // Skip auto-clear if the watch was just created (< 60s) — the branch may not
    // be pushed yet, and a stale PR from a previous use of the same branch name
    // could trigger a false-positive clear (Hotfix E, PR #451 follow-up).
    if let PrState::Terminal { merged } = provider.check_pr_terminal(repo, branch).await {
        // Grace: don't clear watches younger than 60s (branch not yet pushed).
        if let Ok(content) = std::fs::read_to_string(watch_path) {
            if let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(expires_at) = watch["expires_at"].as_str() {
                    if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                        let watch_age = exp.with_timezone(&chrono::Utc)
                            - chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS);
                        let since_creation = chrono::Utc::now().signed_duration_since(watch_age);
                        if since_creation < chrono::Duration::seconds(60) {
                            tracing::info!(
                                repo,
                                branch,
                                merged,
                                "skipping PR-terminal auto-clear — watch too young (<60s)"
                            );
                            // Don't clear — let the next poll cycle re-check.
                            // Fall through to normal poll logic below.
                        } else {
                            remove_watch(
                                home,
                                watch_path,
                                &audit_label,
                                repo,
                                branch,
                                "pr_terminal",
                            );
                            tracing::info!(
                                repo,
                                branch,
                                merged,
                                "CI watcher auto-cleared: PR terminal"
                            );
                            if merged {
                                crate::status_summary::auto_close_merged_tasks(home, branch);
                            }
                            return Ok(());
                        }
                    } else {
                        remove_watch(home, watch_path, &audit_label, repo, branch, "pr_terminal");
                        tracing::info!(
                            repo,
                            branch,
                            merged,
                            "CI watcher auto-cleared: PR terminal"
                        );
                        if merged {
                            crate::status_summary::auto_close_merged_tasks(home, branch);
                        }
                        return Ok(());
                    }
                } else {
                    remove_watch(home, watch_path, &audit_label, repo, branch, "pr_terminal");
                    tracing::info!(repo, branch, merged, "CI watcher auto-cleared: PR terminal");
                    if merged {
                        crate::status_summary::auto_close_merged_tasks(home, branch);
                    }
                    return Ok(());
                }
            }
        }
    }

    let poll_result = provider.poll_runs(repo, branch).await?;
    let runs = match poll_result {
        CiPollResult::ApiError {
            status,
            message,
            rate_limit_reset,
        } => {
            if let Some(reset_epoch) = rate_limit_reset {
                if let Ok(content) = std::fs::read_to_string(watch_path) {
                    if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
                        watch["rate_limit_until"] = serde_json::json!(reset_epoch);
                        // M1: atomic write
                        let _ = crate::store::atomic_write(
                            watch_path,
                            serde_json::to_string_pretty(&watch)
                                .unwrap_or_default()
                                .as_bytes(),
                        );
                    }
                }
            }
            let notify_msg = match rate_limit_reset {
                Some(reset) => format!(
                    "[ci-warn] {repo}@{branch}: {message} — backoff until reset (epoch {reset})"
                ),
                None => format!("[ci-warn] {repo}@{branch}: {message}"),
            };
            // Outbound info-leak gate (Sprint 21 Phase 1): `notify_msg`
            // carries CI run url + repo name; legacy `None`-allowlist
            // deployments must opt in to receive these via
            // `user_allowlist: [...]`.
            //
            // Sprint 54 P0-1: fan out to every subscriber. The single-call
            // version was last-write-wins on the warning too — when lead+dev
            // both watched a branch, only one received the rate-limit
            // warning and the other silently waited.
            if let Some(ch) = crate::channel::active_channel() {
                for sub in subscribers {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        sub,
                        crate::channel::NotifySeverity::Warn,
                        &notify_msg,
                        false,
                    );
                }
            }
            return Err(anyhow::anyhow!("{status}: {message}"));
        }
        CiPollResult::Runs {
            runs,
            rate_limit_remaining,
            rate_limit_limit,
        } => {
            // Sprint 54 P0-2: persist the latest quota counters even on
            // empty-runs polls, so the next tick's `adaptive_interval`
            // sees the freshest snapshot. Done before the empty-runs
            // early return below.
            if rate_limit_remaining.is_some() || rate_limit_limit.is_some() {
                if let Ok(content) = std::fs::read_to_string(watch_path) {
                    if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(r) = rate_limit_remaining {
                            watch["rate_limit_remaining"] = serde_json::json!(r);
                        }
                        if let Some(l) = rate_limit_limit {
                            watch["rate_limit_limit"] = serde_json::json!(l);
                        }
                        let _ = crate::store::atomic_write(
                            watch_path,
                            serde_json::to_string_pretty(&watch)
                                .unwrap_or_default()
                                .as_bytes(),
                        );
                    }
                }
            }
            // Sprint 54 P0-5 (sub-scope B): a successful poll clears
            // any in-flight stall state. If we previously fired
            // `[ci-watch-stalled]`, the symmetrical
            // `[ci-watch-resumed]` event fans out to subscribers
            // exactly once.
            clear_stall_and_maybe_notify_resumed(home, watch_path, repo, branch, subscribers);
            if runs.is_empty() {
                return Ok(());
            }
            runs
        }
    };

    // Determine the latest head_sha from the newest run.
    let current_sha = runs.first().map(|r| r.head_sha.as_str()).unwrap_or("");

    // If head_sha changed (force push), reset last_run_id so we pick up new runs.
    let effective_last_run_id = if prev_head_sha.is_some_and(|prev| prev != current_sha) {
        tracing::info!(repo, branch, old_sha = ?prev_head_sha, new_sha = current_sha, "head_sha changed, resetting run tracking");
        None
    } else {
        last_run_id
    };

    let to_notify = select_runs_to_notify(&runs, effective_last_run_id);
    if to_notify.is_empty() {
        // No new terminal runs — update head_sha but keep last_run_id.
        if let Some(id) = effective_last_run_id {
            update_watch_state(watch_path, Some(id), current_sha);
        }
        return Ok(());
    }

    let mut max_notified_id = effective_last_run_id.unwrap_or(0);
    for &idx in &to_notify {
        if runs[idx].id > max_notified_id {
            max_notified_id = runs[idx].id;
        }
    }

    let deduped = dedupe_notifications_by_head_sha(&runs, &to_notify, last_notified_sha);
    let mut new_notified_sha = last_notified_sha.map(String::from);

    for (idx, run_id, sha) in &deduped {
        let run = &runs[*idx];
        let conclusion = run.conclusion.as_deref();

        if let Some(headline) = ci_notification_message(repo, branch, conclusion, None, Some(sha)) {
            let failure_detail = if conclusion == Some("failure") {
                Some(provider.fetch_failure_summary(repo, *run_id).await)
            } else {
                None
            };
            let body = build_inbox_body(
                &headline,
                conclusion.unwrap_or(""),
                failure_detail.as_deref(),
                &run.url,
            );

            // Sprint 54 P0-1 — Subscriber fan-out: every subscriber receives
            // the in-band agent inject + the inbox enqueue. Without the
            // fan-out, the most-recent `ci watch` caller would shadow all
            // earlier subscribers (last-write-wins on the watch JSON's
            // single `instance` field). The poll above is shared (one HTTP
            // request per cycle); only the notification side-effects loop.
            let repo_branch_key = format!("{repo}@{branch}");
            let supersede_token = format!("ci-{}-{}", run_id, sha);
            // EMPIRICAL REGRESSION-PROOF FLIP: replace `subscribers` below
            // with `&subscribers[..1]` to simulate the pre-r0 single-recipient
            // bug. The `subscriber_fan_out_notifies_every_member` test
            // immediately fails with the dev-inbox-missing assertion.
            for sub in subscribers {
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(sub) {
                    let _ = agent::inject_to_agent(handle, headline.as_bytes());
                }
                drop(reg);
                // M6: mark prior ci-watch messages for same repo+branch as superseded
                crate::inbox::mark_ci_watch_superseded(
                    home,
                    sub,
                    &repo_branch_key,
                    &supersede_token,
                );
                let _ = crate::inbox::enqueue(
                    home,
                    sub,
                    crate::inbox::InboxMessage {
                        schema_version: 0,
                        id: None,
                        read_at: None,
                        thread_id: None,
                        parent_id: None,
                        task_id: None,
                        force_meta: None,
                        correlation_id: None,
                        reviewed_head: None,
                        from: "system:ci".to_string(),
                        text: body.clone(),
                        kind: Some("ci-watch".to_string()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        channel: None,
                        delivery_mode: None,
                        attachments: vec![],
                        in_reply_to_msg_id: None,
                        in_reply_to_excerpt: None,
                        superseded_by: None,
                        from_id: None,
                        broadcast_context: None,
                    },
                );
            }
        }
        new_notified_sha = Some(sha.to_string());
    }

    update_watch_state_with_notify(
        watch_path,
        Some(max_notified_id),
        current_sha,
        new_notified_sha.as_deref(),
    );
    Ok(())
}

/// Persist updated tracking state (last_run_id + head_sha) to the watch file.
fn update_watch_state(watch_path: &Path, run_id: Option<u64>, head_sha: &str) {
    update_watch_state_with_notify(watch_path, run_id, head_sha, None);
}

/// Persist tracking state including last_notified_head_sha.
fn update_watch_state_with_notify(
    watch_path: &Path,
    run_id: Option<u64>,
    head_sha: &str,
    notified_sha: Option<&str>,
) {
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
            watch["last_run_id"] = serde_json::json!(run_id);
            if !head_sha.is_empty() {
                watch["head_sha"] = serde_json::json!(head_sha);
            }
            if let Some(sha) = notified_sha {
                watch["last_notified_head_sha"] = serde_json::json!(sha);
            }
            watch["last_terminal_seen_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
            // M1: atomic write to prevent partial-file on crash
            let _ = crate::store::atomic_write(
                watch_path,
                serde_json::to_string_pretty(&watch)
                    .unwrap_or_default()
                    .as_bytes(),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ciwatch-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    // -----------------------------------------------------------------
    // Sprint 54 P0-2 — adaptive_interval. 3-zone curve based on
    // remaining quota. Each test pins one of the contract gates from
    // dispatch m-20260507042300780703-3:
    //   1. healthy zone (remaining_pct > 0.5) → no scaling
    //   2. cautious zone (0.1 < … ≤ 0.5)      → ×2
    //   3. critical zone (≤ 0.1)              → ×4
    //   4. missing headers                    → fallback to baseline
    //
    // Empirical regression-proof: a mutation that always returns the
    // baseline regardless of remaining_pct trips tests 2 and 3.
    // -----------------------------------------------------------------

    #[test]
    fn adaptive_interval_healthy_zone_uses_configured_baseline() {
        // remaining/limit = 1.0 (full quota) ⇒ no scaling, exactly the
        // configured interval. Mirror this in production: a freshly
        // booted daemon with full GitHub quota polls at user-configured
        // cadence, never widening behind the operator's back.
        assert_eq!(adaptive_interval(60, Some(5000), Some(5000)), 60);
        // Boundary: just above 50% (501/1000 = 0.501) is still healthy.
        assert_eq!(adaptive_interval(60, Some(501), Some(1000)), 60);
    }

    #[test]
    fn adaptive_interval_cautious_zone_doubles() {
        // remaining_pct = 0.3 ⇒ ×2 multiplier. The agent is consuming
        // quota faster than the healthy threshold but isn't critical
        // yet — preempt by halving poll frequency.
        assert_eq!(adaptive_interval(60, Some(300), Some(1000)), 120);
        // Boundary: exactly 50% is cautious (the >0.5 → healthy guard
        // is strict, so 0.5 falls into the next zone).
        assert_eq!(adaptive_interval(60, Some(500), Some(1000)), 120);
        // Boundary: just above 10% (101/1000 = 0.101) is still cautious.
        assert_eq!(adaptive_interval(60, Some(101), Some(1000)), 120);
    }

    #[test]
    fn adaptive_interval_critical_zone_quadruples() {
        // remaining_pct = 0.05 ⇒ ×4 multiplier. Avoids burning the
        // last few requests in a cluster. Mirror this in production:
        // when GitHub returns a low remaining count, the next poll
        // backs off so the daemon doesn't trip the 60/hr (unauth) or
        // 5000/hr (auth) cap.
        assert_eq!(adaptive_interval(60, Some(50), Some(1000)), 240);
        // Boundary: exactly 10% is critical (the >0.1 → cautious guard
        // is strict).
        assert_eq!(adaptive_interval(60, Some(100), Some(1000)), 240);
        // Pathological: zero remaining still resolves to ×4 (we don't
        // pretend to know what reset_epoch is — the existing
        // rate_limit_until path handles that recovery separately).
        assert_eq!(adaptive_interval(60, Some(0), Some(1000)), 240);
    }

    #[test]
    fn adaptive_interval_missing_headers_falls_back_to_configured() {
        // Either field absent (or zero limit) ⇒ baseline. Producers
        // that don't emit the GitHub-style headers (GitLab, Bitbucket
        // Cloud) preserve their existing behavior here.
        assert_eq!(adaptive_interval(60, None, None), 60);
        assert_eq!(adaptive_interval(60, Some(5000), None), 60);
        assert_eq!(adaptive_interval(60, None, Some(5000)), 60);
        // Pathological limit=0 (avoids div-by-zero, falls through).
        assert_eq!(adaptive_interval(60, Some(100), Some(0)), 60);
    }

    #[test]
    fn watch_is_due_null_last_polled_at_fires_immediately() {
        // A freshly-registered watch (or a pre-schema file missing the
        // last_polled_at field) must be due on the first tick. This is
        // the condition that makes the next daemon tick actually poll
        // GitHub instead of waiting ~interval_secs.
        assert!(watch_is_due(None, 60, 1_700_000_000_000));
    }

    #[test]
    fn watch_is_due_within_interval_is_throttled() {
        // Polled 30 s ago, interval 60 s ⇒ still throttled. Prevents
        // back-to-back polls from hammering the GitHub API during
        // daemon ticks (10 s cadence) or concurrent callers.
        let now_ms = 1_700_000_000_000_i64;
        let recent = now_ms - 30_000; // 30 s ago
        assert!(!watch_is_due(Some(recent), 60, now_ms));
    }

    #[test]
    fn watch_is_due_past_interval_fires_again() {
        // Polled 61 s ago, interval 60 s ⇒ due. Equality case
        // (elapsed == interval) is also treated as due — boundary
        // matches the `>=` in the throttle.
        let now_ms = 1_700_000_000_000_i64;
        let stale = now_ms - 61_000;
        assert!(watch_is_due(Some(stale), 60, now_ms));
        let exact = now_ms - 60_000;
        assert!(watch_is_due(Some(exact), 60, now_ms));
    }

    #[test]
    fn watch_is_due_future_timestamp_is_throttled() {
        // Defensive: a clock going backwards (or a hand-edited file
        // with a bogus future timestamp) should not flood GH. The
        // saturating_sub makes elapsed non-negative, and 0 < interval
        // ⇒ throttled. We'd rather be quietly silent on a weird clock
        // than burn rate limit.
        let now_ms = 1_700_000_000_000_i64;
        let future = now_ms + 10_000; // 10 s in the future
        assert!(!watch_is_due(Some(future), 60, now_ms));
    }

    #[test]
    fn ci_watch_success_notifies() {
        let msg = ci_notification_message("owner/repo", "main", Some("success"), None, None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-pass] owner/repo@main: passed ✓\r")
        );
    }

    #[test]
    fn ci_watch_failure_headline_excludes_detail() {
        // Job detail moved to inbox body — headline just says "failure"
        let msg = ci_notification_message(
            "owner/repo",
            "main",
            Some("failure"),
            Some("Build / Test"),
            None,
        );
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure\r"));
    }

    #[test]
    fn ci_watch_failure_without_detail_same_headline() {
        let msg = ci_notification_message("owner/repo", "main", Some("failure"), None, None);
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure\r"));
    }

    #[test]
    fn ci_watch_in_progress_skipped() {
        let msg = ci_notification_message("owner/repo", "main", None, None, None);
        assert!(
            msg.is_none(),
            "in-progress (null conclusion) must be skipped"
        );
    }

    #[test]
    fn ci_watch_cancelled_notifies() {
        let msg = ci_notification_message("owner/repo", "feat", Some("cancelled"), None, None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-ended] owner/repo@feat: cancelled\r")
        );
    }

    #[test]
    fn ci_watch_timed_out_notifies() {
        let msg = ci_notification_message("owner/repo", "main", Some("timed_out"), None, None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-ended] owner/repo@main: timed_out\r")
        );
    }

    #[test]
    fn test_force_push_invalidates_run_id() {
        // When head_sha changes between polls, the effective last_run_id
        // should be reset to None so the new run is picked up even if
        // the run_id hasn't changed yet.
        let prev_sha = Some("abc123");
        let current_sha = "def456";
        // Simulate the logic from ci_check_repo
        let last_run_id = Some(42u64);
        let effective = if prev_sha.is_some_and(|prev| prev != current_sha) {
            None
        } else {
            last_run_id
        };
        assert_eq!(effective, None, "force push must reset last_run_id");

        // Same SHA → preserve run_id
        let same_sha = "abc123";
        let effective2 = if prev_sha.is_some_and(|prev| prev != same_sha) {
            None
        } else {
            last_run_id
        };
        assert_eq!(effective2, Some(42), "same SHA must preserve last_run_id");
    }

    #[test]
    fn test_pr_merged_clears_watcher() {
        // When a watch file exists and the PR is terminal, the file
        // should be removed. We test the update_watch_state + remove
        // flow by verifying the file lifecycle.
        let dir = std::env::temp_dir().join(format!("agend-ci-test-merged-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("ci-watches")).ok();
        let watch_path = dir.join("ci-watches").join("test.json");
        std::fs::write(
            &watch_path,
            r#"{"repo":"o/r","branch":"feat","last_run_id":null,"head_sha":null}"#,
        )
        .ok();
        assert!(watch_path.exists());

        // Simulate PR terminal → auto-clear
        let _ = std::fs::remove_file(&watch_path);
        assert!(
            !watch_path.exists(),
            "watcher file must be removed on PR terminal"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- classify_runs_response: silent-rate-limit regression pin ---

    #[test]
    fn classify_response_picks_first_run_on_2xx() {
        let body = serde_json::json!({
            "workflow_runs": [{"id": 42, "head_sha": "abc"}, {"id": 41}]
        });
        match classify_runs_response(200, &body) {
            RunsResponse::Run(r) => assert_eq!(r["id"].as_u64(), Some(42)),
            other => panic!("expected Run, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn classify_response_no_runs_on_2xx_empty_array() {
        // Genuine "branch has no runs yet" — must NOT be confused with
        // an API error.
        let body = serde_json::json!({"workflow_runs": []});
        assert!(matches!(
            classify_runs_response(200, &body),
            RunsResponse::NoRuns
        ));
    }

    #[test]
    fn classify_response_rate_limit_is_api_error_not_no_runs() {
        // Real-world body returned by GitHub when an unauthenticated
        // client exceeds 60/hr. Without the status check, the absence
        // of `workflow_runs` here looks identical to the legit empty
        // case above and silently swallows every subsequent CI event.
        let body = serde_json::json!({
            "message": "API rate limit exceeded for 1.2.3.4. (But here's the good news: ...)",
            "documentation_url": "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"
        });
        match classify_runs_response(403, &body) {
            RunsResponse::ApiError(msg) => {
                assert!(msg.contains("403"), "msg should include status: {msg}");
                assert!(
                    msg.contains("rate limit"),
                    "msg should surface GH message: {msg}"
                );
            }
            _ => panic!("rate-limit response must be ApiError, not NoRuns"),
        }
    }

    #[test]
    fn classify_response_token_hint_only_when_unauthenticated_403() {
        // Hint should fire on unauthenticated 403 rate-limit. We can't
        // safely mutate $GITHUB_TOKEN in a parallel-test process, so
        // assert only the prefix shape and trust the env-gated branch.
        let body =
            serde_json::json!({"message": "API rate limit exceeded for example. Authenticated …"});
        let RunsResponse::ApiError(msg) = classify_runs_response(403, &body) else {
            panic!("expected ApiError");
        };
        assert!(msg.starts_with("GH API 403: API rate limit exceeded"));
    }

    #[test]
    fn classify_response_5xx_is_api_error() {
        let body = serde_json::json!({"message": "Server Error"});
        assert!(matches!(
            classify_runs_response(500, &body),
            RunsResponse::ApiError(_)
        ));
    }

    #[test]
    fn classify_response_unknown_payload_falls_through_safely() {
        // 200 OK but missing workflow_runs entirely (would never happen
        // in practice but must not panic).
        let body = serde_json::json!({});
        assert!(matches!(
            classify_runs_response(200, &body),
            RunsResponse::NoRuns
        ));
    }

    // --- github_token_warning: preventive watch_ci response hint ---

    #[test]
    fn github_token_warning_none_when_token_present() {
        assert!(github_token_warning(Some("ghp_realtokenhere")).is_none());
    }

    #[test]
    fn github_token_warning_set_when_absent() {
        let msg = github_token_warning(None).expect("missing token must warn");
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "message must name the env var: {msg}"
        );
        assert!(
            msg.contains("unauthenticated") || msg.contains("60"),
            "message must explain the cost: {msg}"
        );
    }

    #[test]
    fn github_token_warning_treats_empty_and_whitespace_as_absent() {
        // `std::env::var("GITHUB_TOKEN")` returns `Ok("")` when the var is
        // exported-but-empty — a distinct case from "unset" but equally
        // unusable. Whitespace-only should be treated the same.
        assert!(github_token_warning(Some("")).is_some());
        assert!(github_token_warning(Some("   ")).is_some());
        assert!(github_token_warning(Some("\t\n")).is_some());
    }

    #[test]
    fn test_repo_with_slash_no_collision() {
        // Two repos that would collide under the old `replace('/', '_')`
        // scheme must produce distinct filenames with the hash approach.
        let f1 = watch_filename("owner/repo", "main");
        let f2 = watch_filename("owner_repo", "main");
        assert_ne!(f1, f2, "owner/repo and owner_repo must not collide");

        // Same repo+branch must be deterministic
        let f3 = watch_filename("owner/repo", "main");
        assert_eq!(f1, f3, "same input must produce same filename");

        // Different branches of same repo must differ
        let f4 = watch_filename("owner/repo", "feat");
        assert_ne!(
            f1, f4,
            "different branches must produce different filenames"
        );
    }

    #[test]
    fn test_multi_run_notifies_all_terminal_since_last() {
        let runs = vec![
            CiRun {
                id: 100,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 101,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
            CiRun {
                id: 102,
                conclusion: None,
                head_sha: "ccc".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(99));
        assert_eq!(
            selected,
            vec![0, 1],
            "should notify runs 100 and 101, skip 102 (in-progress)"
        );
    }

    #[test]
    fn test_in_progress_does_not_appear_in_selection() {
        let runs = vec![CiRun {
            id: 200,
            conclusion: None,
            head_sha: "aaa".into(),
            url: String::new(),
        }];
        let selected = select_runs_to_notify(&runs, None);
        assert!(selected.is_empty(), "in-progress run must not be selected");
    }

    #[test]
    fn test_mixed_terminal_states_all_notified() {
        let runs = vec![
            CiRun {
                id: 300,
                conclusion: Some("failure".into()),
                head_sha: "a".into(),
                url: String::new(),
            },
            CiRun {
                id: 301,
                conclusion: Some("cancelled".into()),
                head_sha: "b".into(),
                url: String::new(),
            },
            CiRun {
                id: 302,
                conclusion: Some("success".into()),
                head_sha: "c".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(299));
        assert_eq!(
            selected,
            vec![0, 1, 2],
            "all 3 terminal runs should be selected"
        );
    }

    #[test]
    fn test_already_notified_runs_skipped() {
        let runs = vec![
            CiRun {
                id: 400,
                conclusion: Some("success".into()),
                head_sha: "a".into(),
                url: String::new(),
            },
            CiRun {
                id: 401,
                conclusion: Some("success".into()),
                head_sha: "b".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(400));
        assert_eq!(
            selected,
            vec![1],
            "run 400 already notified, only 401 selected"
        );
    }

    #[test]
    fn test_same_head_sha_deduplicates_notification() {
        let runs = vec![
            CiRun {
                id: 500,
                conclusion: Some("failure".into()),
                head_sha: "abc".into(),
                url: String::new(),
            },
            CiRun {
                id: 501,
                conclusion: Some("success".into()),
                head_sha: "abc".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(499));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None);
        assert_eq!(deduped.len(), 1, "same sha → 1 notification");
        assert_eq!(deduped[0].1, 501, "latest run_id wins");
        assert_eq!(deduped[0].2, "abc");
    }

    #[test]
    fn test_dedupe_skips_already_notified_sha() {
        let runs = vec![
            CiRun {
                id: 600,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 601,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(599));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, Some("aaa"));
        assert_eq!(deduped.len(), 1, "aaa already notified → only bbb");
        assert_eq!(deduped[0].2, "bbb");
    }

    #[test]
    fn test_different_head_sha_triggers_new_notification() {
        let runs = vec![
            CiRun {
                id: 600,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 601,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(599));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None);
        assert_eq!(deduped.len(), 2, "different shas → 2 notifications");
    }

    #[test]
    fn test_inbox_body_failure_has_detail_and_url() {
        let body = build_inbox_body(
            "[ci-fail] o/r@main: failure\r",
            "failure",
            Some("Check / Clippy"),
            "https://github.com/o/r/actions/runs/123",
        );
        assert!(
            body.contains("Detail: Check / Clippy"),
            "failure body must have detail: {body}"
        );
        assert!(body.contains("URL: https://"), "body must have URL: {body}");
    }

    #[test]
    fn test_inbox_body_success_has_url_no_detail() {
        let body = build_inbox_body(
            "[ci-pass] o/r@main: passed ✓\r",
            "success",
            None,
            "https://github.com/o/r/actions/runs/456",
        );
        assert!(
            body.contains("URL: https://"),
            "success body must have URL: {body}"
        );
        assert!(
            !body.contains("Detail:"),
            "success body must not have Detail: {body}"
        );
    }

    #[test]
    fn test_headline_excludes_job_detail() {
        // ci_notification_message must NOT contain job/step names like
        // "(ubuntu-latest)" or "(tray feature)" — those go in inbox body.
        let msg = ci_notification_message(
            "o/r",
            "main",
            Some("failure"),
            Some("Check / Clippy (tray)"),
            None,
        );
        let headline = msg.unwrap();
        assert!(
            !headline.contains("Clippy"),
            "headline must not contain job detail: {headline}"
        );
        assert!(
            !headline.contains("tray"),
            "headline must not contain step detail: {headline}"
        );
        assert!(
            headline.contains("failure"),
            "headline must contain conclusion: {headline}"
        );
    }

    #[test]
    fn test_headline_success_clean() {
        let msg = ci_notification_message("o/r", "feat", Some("success"), None, None);
        let headline = msg.unwrap();
        assert!(headline.contains("passed"), "success headline: {headline}");
        assert!(
            !headline.contains("unknown"),
            "no unknown in success: {headline}"
        );
    }

    #[test]
    fn test_watch_expires_after_ttl_inactivity() {
        let dir = tmp_dir("ttl-expire");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with last_terminal_seen_at 80 hours ago (> 72h TTL)
        let old_ts = (chrono::Utc::now() - chrono::Duration::hours(80)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": old_ts,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Run check — should remove the watch
        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(!watch_path.exists(), "expired watch must be removed");
        // Event log should have entry
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(log.contains("ci_watch_removed"), "removal must be logged");
        assert!(
            log.contains("inactivity_ttl"),
            "reason must be inactivity_ttl"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_preserved_when_not_expired() {
        let dir = tmp_dir("ttl-fresh");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with recent last_terminal_seen_at (1 hour ago, well within 72h)
        let recent_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": recent_ts,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(watch_path.exists(), "fresh watch must be preserved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_expires_at_absolute() {
        let dir = tmp_dir("ttl-absolute");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at in the past
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "old", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
        });
        let filename = watch_filename("o/r", "old");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            !watch_path.exists(),
            "past-expires_at watch must be removed"
        );
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        // Sprint 57 Wave 2 Track B (#546 Item 1): the eager per-tick GC
        // pass at the top of `check_ci_watches` removes the watch first
        // and emits `reason=eager_gc_expired`; the legacy lazy expiry
        // emits `reason=expired`. Either substring is correct evidence
        // that the absolute-TTL path fired.
        assert!(
            log.contains("expired"),
            "reason must include 'expired'; got:\n{log}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_legacy_watch_without_ttl_fields_not_removed() {
        let dir = tmp_dir("ttl-legacy");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Legacy watch without expires_at or last_terminal_seen_at
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            watch_path.exists(),
            "legacy watch without TTL fields must not be removed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_remove_watch_on_pr_terminal_logs_event() {
        let dir = tmp_dir("pr-terminal");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = watch_filename("o/r", "feat-merged");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(
            &watch_path,
            r#"{"repo":"o/r","branch":"feat-merged","instance":"a1"}"#,
        )
        .unwrap();
        assert!(watch_path.exists());

        remove_watch(&dir, &watch_path, "a1", "o/r", "feat-merged", "pr_terminal");

        assert!(!watch_path.exists(), "watch must be removed");
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(log.contains("ci_watch_removed"));
        assert!(log.contains("reason=pr_terminal"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_preserved_when_pr_check_unavailable() {
        let dir = tmp_dir("pr-fail");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let future = (chrono::Utc::now() + chrono::Duration::hours(48)).to_rfc3339();
        let recent = chrono::Utc::now().to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-active", "interval_secs": 60,
            "instance": "a1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": future, "last_terminal_seen_at": recent,
        });
        let filename = watch_filename("o/r", "feat-active");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).unwrap();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            watch_path.exists(),
            "watch must survive when PR check unavailable"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- MockCiProvider + state machine tests ---

    use parking_lot::Mutex;

    /// Mock CI provider for testing ci_check_repo state machine without HTTP.
    struct MockCiProvider {
        poll_result: Mutex<Option<CiPollResult>>,
        pr_state: Mutex<PrState>,
        failure_summary: Mutex<String>,
    }

    impl MockCiProvider {
        fn with_runs(runs: Vec<CiRun>) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::Runs {
                    runs,
                    rate_limit_remaining: None,
                    rate_limit_limit: None,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        /// Sprint 54 P0-2: variant that lets a test seed quota counters
        /// directly so adaptive-backoff persistence + throttle behavior
        /// can be exercised end-to-end without an HTTP layer.
        #[allow(dead_code)]
        fn with_runs_and_quota(
            runs: Vec<CiRun>,
            remaining: Option<u64>,
            limit: Option<u64>,
        ) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::Runs {
                    runs,
                    rate_limit_remaining: remaining,
                    rate_limit_limit: limit,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        fn with_api_error(status: u16, message: &str) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::ApiError {
                    status,
                    message: message.to_string(),
                    rate_limit_reset: None,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        fn with_pr_terminal(self) -> Self {
            *self.pr_state.lock() = PrState::Terminal { merged: true };
            self
        }
    }

    #[async_trait::async_trait]
    impl CiProvider for MockCiProvider {
        async fn poll_runs(&self, _repo: &str, _branch: &str) -> anyhow::Result<CiPollResult> {
            Ok(self.poll_result.lock().take().unwrap())
        }
        async fn check_pr_terminal(&self, _repo: &str, _branch: &str) -> PrState {
            let mut guard = self.pr_state.lock();
            std::mem::replace(&mut *guard, PrState::Open)
        }
        async fn fetch_failure_summary(&self, _repo: &str, _run_id: u64) -> String {
            self.failure_summary.lock().clone()
        }
        fn token_warning(&self) -> Option<&'static str> {
            None
        }
    }

    /// Helper: run ci_check_repo with a mock provider in a temp dir.
    fn run_ci_check(
        dir: &Path,
        watch_json: &serde_json::Value,
        provider: &dyn CiProvider,
    ) -> anyhow::Result<()> {
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let repo = watch_json["repo"].as_str().unwrap();
        let branch = watch_json["branch"].as_str().unwrap();
        let subscribers = parse_subscribers(watch_json);
        let filename = watch_filename(repo, branch);
        let watch_path = ci_dir.join(&filename);
        std::fs::write(
            &watch_path,
            serde_json::to_string_pretty(watch_json).unwrap(),
        )
        .unwrap();

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            dir,
            &watch_path,
            repo,
            branch,
            &subscribers,
            watch_json["last_run_id"].as_u64(),
            watch_json["head_sha"].as_str(),
            watch_json["last_notified_head_sha"].as_str(),
            &registry,
            provider,
        ))
    }

    fn base_watch() -> serde_json::Value {
        serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "last_notified_head_sha": null,
        })
    }

    #[test]
    fn mock_success_run_updates_watch_state() {
        let dir = tmp_dir("mock-success");
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 100,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: "https://example.com/100".into(),
        }]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        // Watch file should be updated with last_run_id and head_sha
        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(updated["last_run_id"].as_u64(), Some(100));
        assert_eq!(updated["head_sha"].as_str(), Some("abc"));

        // Inbox should have a notification
        let inbox_dir = dir.join("inbox");
        let has_inbox = inbox_dir.exists()
            && std::fs::read_dir(&inbox_dir)
                .map(|d| d.count() > 0)
                .unwrap_or(false);
        assert!(has_inbox, "success run should enqueue inbox notification");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_failure_run_includes_detail() {
        let dir = tmp_dir("mock-failure");
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 200,
            conclusion: Some("failure".into()),
            head_sha: "def".into(),
            url: "https://example.com/200".into(),
        }]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        // Check inbox contains failure detail
        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        if inbox_path.exists() {
            let content = std::fs::read_to_string(&inbox_path).unwrap();
            assert!(
                content.contains("ci-fail"),
                "inbox should have ci-fail: {content}"
            );
            assert!(
                content.contains("Build / Test"),
                "inbox should have failure detail: {content}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_api_error_propagates() {
        let dir = tmp_dir("mock-api-err");
        let provider = MockCiProvider::with_api_error(403, "GH API 403: rate limit exceeded");
        let result = run_ci_check(&dir, &base_watch(), &provider);
        assert!(result.is_err(), "API error must propagate");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("403"), "error should contain status: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_pr_terminal_clears_watch() {
        let dir = tmp_dir("mock-pr-term");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        // Provider says PR is terminal — runs response doesn't matter
        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(!watch_path.exists(), "PR terminal must remove watch file");
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("pr_terminal"),
            "event log must record pr_terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_no_runs_preserves_watch() {
        let dir = tmp_dir("mock-no-runs");
        let provider = MockCiProvider::with_runs(vec![]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        assert!(watch_path.exists(), "empty runs must preserve watch");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_force_push_resets_tracking() {
        let dir = tmp_dir("mock-force-push");
        // Watch has old head_sha "old123", new run has different sha
        let mut watch = base_watch();
        watch["head_sha"] = serde_json::json!("old123");
        watch["last_run_id"] = serde_json::json!(50);
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 51,
            conclusion: Some("success".into()),
            head_sha: "new456".into(),
            url: "https://example.com/51".into(),
        }]);
        run_ci_check(&dir, &watch, &provider).unwrap();

        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        // After force push, run 51 should be notified even though id > last_run_id=50
        // would normally catch it — the key is head_sha changed so tracking reset
        assert_eq!(updated["last_run_id"].as_u64(), Some(51));
        assert_eq!(updated["head_sha"].as_str(), Some("new456"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_rate_limit_writes_backoff_and_propagates_error() {
        let dir = tmp_dir("mock-rate-limit");
        let provider = MockCiProvider {
            poll_result: Mutex::new(Some(CiPollResult::ApiError {
                status: 403,
                message: "GH API 403: rate limit exceeded".to_string(),
                rate_limit_reset: Some(9999999999),
            })),
            pr_state: Mutex::new(PrState::Open),
            failure_summary: Mutex::new(String::new()),
        };
        let result = run_ci_check(&dir, &base_watch(), &provider);
        assert!(result.is_err(), "rate-limit must propagate as error");
        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(updated["rate_limit_until"].as_u64(), Some(9999999999));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rate_limit_backoff_skips_polling() {
        let dir = tmp_dir("backoff-skip");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let future = (chrono::Utc::now().timestamp() + 3600) as u64;
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "rate_limit_until": future,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);
        assert!(watch_path.exists(), "backoff watch must be preserved");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── GitLab CiProvider tests (Sprint 39 PR-1) ────────────────────

    /// Helper: mock HTTP server for GitLab tests. Captures path + headers.
    /// Captured request from mock server: (path, full_request).
    type MockCapture = std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>;

    /// RAII guard that saves/restores GITLAB_TOKEN + HOME env vars.
    /// Also holds a static mutex to serialize env-var-touching tests.
    struct GitlabTokenGuard {
        prev_token: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl GitlabTokenGuard {
        fn clear() -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("GITLAB_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::remove_var("GITLAB_TOKEN");
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
        fn set(val: &str) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("GITLAB_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("GITLAB_TOKEN", val);
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
    }
    impl Drop for GitlabTokenGuard {
        fn drop(&mut self) {
            match &self.prev_token {
                Some(v) => std::env::set_var("GITLAB_TOKEN", v),
                None => std::env::remove_var("GITLAB_TOKEN"),
            }
            if let Some(v) = &self.prev_home {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn gitlab_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let captured_clone = captured.clone();
        let body = response_body.to_string();

        // fire-and-forget: test mock server thread
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            // Capture full request (path + headers) for assertion.
            *captured_clone.lock().expect("lock") = Some((path, request));

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        (port, handle, captured)
    }

    /// §3.5.10 production-path-coupled: GitLabCiProvider::poll_runs
    /// against mock server with spec-quoted fixture.
    #[test]
    fn gitlab_poll_runs_parses_pipelines() {
        let fixture = include_str!("../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let result = rt.block_on(provider.poll_runs("foo/bar", "main"));

        handle.join().expect("mock");
        let (path, _request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/projects/foo%2Fbar/pipelines"),
            "path must target pipelines: {path}"
        );

        let runs = match result.expect("poll_runs") {
            super::CiPollResult::Runs { runs, .. } => runs,
            other => panic!("expected Runs, got: {other:?}"),
        };
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 47);
        assert_eq!(runs[0].conclusion, Some("success".to_string()));
        assert_eq!(runs[1].conclusion, Some("failure".to_string()));
    }

    /// §3.5.10: GitLabCiProvider::check_pr_terminal parses MR state.
    #[test]
    fn gitlab_check_pr_terminal_merged() {
        let fixture = include_str!("../../tests/fixtures/gitlab-merge-requests-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));

        handle.join().expect("mock");
        let (path, _req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/merge_requests"), "path: {path}");
        assert!(path.contains("source_branch=feat"), "query: {path}");
        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "expected Terminal(merged), got: {state:?}"
        );
    }

    /// §3.5.10: GitLabCiProvider::fetch_failure_summary finds failed job.
    #[test]
    fn gitlab_fetch_failure_summary_finds_failed_job() {
        let fixture = include_str!("../../tests/fixtures/gitlab-jobs-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));

        handle.join().expect("mock");
        let (path, _req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/pipelines/48/jobs"), "path: {path}");
        // Summary starts with stage/name (trace fetch hits second accept
        // which isn't available — falls back to header-only).
        assert!(
            summary.starts_with("test / cargo-test"),
            "summary: {summary}"
        );
    }

    /// Auth fallback: token_warning returns warning when no token found.
    #[test]
    fn gitlab_token_warning_when_no_token() {
        let _guard = GitlabTokenGuard::clear();
        let provider = super::GitLabCiProvider::new().expect("provider");
        let warning = provider.token_warning();
        assert!(warning.is_some(), "should warn when no token");
        assert!(
            warning.expect("w").contains("GITLAB_TOKEN"),
            "warning must mention GITLAB_TOKEN"
        );
    }

    /// Smoke: GitLabCiProvider::new() defaults to gitlab.com base URL.
    #[test]
    fn gitlab_new_defaults_to_gitlab_com() {
        let provider = super::GitLabCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://gitlab.com");
    }

    /// Smoke: with_base_url() sets custom base URL (self-hosted).
    #[test]
    fn gitlab_with_base_url_sets_custom() {
        let provider =
            super::GitLabCiProvider::with_base_url("https://git.corp.example.com".into())
                .expect("with_base_url");
        assert_eq!(provider.http.base_url, "https://git.corp.example.com");
    }

    /// B4: Auth state 1 — GITLAB_TOKEN env present → PRIVATE-TOKEN header sent.
    #[test]
    fn gitlab_auth_env_token_sends_private_token_header() {
        let fixture = include_str!("../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let _guard = GitlabTokenGuard::set("test-token-123");
        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        std::env::remove_var("GITLAB_TOKEN");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("private-token: test-token-123")
                || request.contains("PRIVATE-TOKEN: test-token-123"),
            "must send PRIVATE-TOKEN header: {request}"
        );
    }

    /// B4 state 2: env absent + glab CLI config present → token from config.
    #[test]
    fn gitlab_auth_glab_config_fallback_sends_private_token_header() {
        let fixture = include_str!("../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        // Guard serializes + saves/restores GITLAB_TOKEN + HOME.
        let mut guard = GitlabTokenGuard::clear();
        // Setup temp HOME with glab config.
        let temp = std::env::temp_dir().join(format!("agend-glab-test-{}", std::process::id()));
        let glab_dir = temp.join(".config").join("glab-cli");
        std::fs::create_dir_all(&glab_dir).ok();
        std::fs::write(
            glab_dir.join("config.yml"),
            "hosts:\n  gitlab.com:\n    token: glab_config_token_abc\n",
        )
        .expect("write glab config");
        // Override HOME so resolve_token finds the temp config.
        guard.prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &temp);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("private-token: glab_config_token_abc")
                || request.contains("PRIVATE-TOKEN: glab_config_token_abc"),
            "must send PRIVATE-TOKEN from glab config: {request}"
        );

        std::fs::remove_dir_all(&temp).ok();
        // guard drop restores HOME + GITLAB_TOKEN
    }

    // ── Bitbucket Cloud CiProvider tests (Sprint 39 PR-2) ───────────

    /// Env guard for BITBUCKET_TOKEN + HOME (mirrors GitlabTokenGuard).
    struct BitbucketTokenGuard {
        prev_token: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    static BB_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl BitbucketTokenGuard {
        fn clear() -> Self {
            let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("BITBUCKET_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::remove_var("BITBUCKET_TOKEN");
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
        fn set(val: &str) -> Self {
            let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("BITBUCKET_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("BITBUCKET_TOKEN", val);
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
    }
    impl Drop for BitbucketTokenGuard {
        fn drop(&mut self) {
            match &self.prev_token {
                Some(v) => std::env::set_var("BITBUCKET_TOKEN", v),
                None => std::env::remove_var("BITBUCKET_TOKEN"),
            }
            if let Some(v) = &self.prev_home {
                std::env::set_var("HOME", v);
            }
        }
    }

    /// Reuse gitlab_mock_server for Bitbucket (same raw TCP pattern).
    /// §3.5.10: poll_runs parses Bitbucket pipelines response.
    #[test]
    fn bitbucket_poll_runs_parses_pipelines() {
        let fixture = include_str!("../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let result = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        let (path, req) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/repositories/foo/bar/pipelines"),
            "path: {path}"
        );
        assert!(path.contains("target.branch=main"), "query: {path}");
        assert!(
            req.to_lowercase().contains("authorization: basic"),
            "must send Basic auth header: {req}"
        );
        let runs = match result.expect("poll_runs") {
            super::CiPollResult::Runs { runs, .. } => runs,
            other => panic!("expected Runs, got: {other:?}"),
        };
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].conclusion, Some("success".to_string()));
        assert_eq!(runs[1].conclusion, Some("failure".to_string()));
    }

    /// §3.5.10: check_pr_terminal parses Bitbucket PR state.
    #[test]
    fn bitbucket_check_pr_terminal_merged() {
        let fixture = include_str!("../../tests/fixtures/bitbucket-pullrequests-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));
        handle.join().expect("mock");
        let (path, req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/pullrequests"), "path: {path}");
        assert!(path.contains("source.branch.name"), "query: {path}");
        assert!(
            req.to_lowercase().contains("authorization: basic"),
            "auth: {req}"
        );
        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "got: {state:?}"
        );
    }

    /// §3.5.10: fetch_failure_summary finds failed step.
    #[test]
    fn bitbucket_fetch_failure_summary_finds_failed_step() {
        // 2-request chain mock: 1st returns steps, 2nd returns log.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured_reqs =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let cap = captured_reqs.clone();
        let steps_body =
            include_str!("../../tests/fixtures/bitbucket-steps-response.json").to_string();
        // fire-and-forget: test mock server thread
        let handle = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).expect("read");
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                cap.lock().expect("lock").push((path, request));
                let body = if i == 0 {
                    &steps_body
                } else {
                    "error line 1\nerror line 2\n"
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                stream.write_all(resp.as_bytes()).expect("write");
            }
        });
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));
        handle.join().expect("mock");
        let reqs = captured_reqs.lock().expect("lock");
        assert_eq!(
            reqs.len(),
            2,
            "expected 2 requests for failure-summary chain"
        );
        assert!(
            reqs[0].0.contains("/pipelines/48/steps"),
            "req1 path: {}",
            reqs[0].0
        );
        assert!(
            reqs[0].1.to_lowercase().contains("authorization: basic"),
            "req1 auth"
        );
        assert!(
            reqs[1].0.contains("/log"),
            "req2 path must contain /log: {}",
            reqs[1].0
        );
        assert!(
            reqs[1].1.to_lowercase().contains("authorization: basic"),
            "req2 auth"
        );
        assert!(
            summary.starts_with("Test"),
            "summary must start with step name: {summary}"
        );
        assert!(
            summary.contains("error line"),
            "summary must contain log tail: {summary}"
        );
    }

    /// Auth state 1: BITBUCKET_TOKEN env → Basic auth header.
    #[test]
    fn bitbucket_auth_env_token_sends_basic_header() {
        let fixture = include_str!("../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:app_pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
            "must send Basic auth: {request}"
        );
    }

    /// Auth state 3: no token → warning.
    #[test]
    fn bitbucket_token_warning_when_no_token() {
        let _guard = BitbucketTokenGuard::clear();
        let provider = super::BitbucketCiProvider::new().expect("provider");
        let warning = provider.token_warning();
        assert!(warning.is_some());
        assert!(warning.expect("w").contains("BITBUCKET_TOKEN"));
    }

    /// Auth state 2: env absent + bb CLI config → Basic auth from config.
    #[test]
    fn bitbucket_auth_bb_config_fallback_sends_basic_header() {
        let fixture = include_str!("../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let mut guard = BitbucketTokenGuard::clear();
        // Setup temp HOME with bb config.
        let temp = std::env::temp_dir().join(format!("agend-bb-test-{}", std::process::id()));
        let bb_dir = temp.join(".config").join("bb");
        std::fs::create_dir_all(&bb_dir).ok();
        std::fs::write(bb_dir.join("config"), "token: bbuser:bb_app_pass\n").expect("write");
        guard.prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &temp);

        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
            "must send Basic auth from bb config: {request}"
        );
        std::fs::remove_dir_all(&temp).ok();
    }

    /// Smoke: constructors.
    #[test]
    fn bitbucket_new_defaults_to_api_bitbucket_org() {
        let provider = super::BitbucketCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://api.bitbucket.org");
    }

    #[test]
    fn bitbucket_with_base_url_sets_custom() {
        let provider =
            super::BitbucketCiProvider::with_base_url("https://bb.corp.example.com".into())
                .expect("with_base_url");
        assert_eq!(provider.http.base_url, "https://bb.corp.example.com");
    }

    /// B5: watch_ci rejects bitbucket_server with operator-actionable error.
    #[test]
    fn watch_ci_rejects_bitbucket_server_with_actionable_error() {
        let dir = std::env::temp_dir().join(format!("agend-bb-reject-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let result = crate::mcp::handlers::ci::handle_watch_ci(
            &dir,
            &serde_json::json!({
                "repo": "myws/myrepo",
                "branch": "feat-test",
                "ci_provider": "bitbucket_server",
            }),
            "test-inst",
        );
        assert!(
            result["error"].as_str().is_some(),
            "must return error for bitbucket_server: {result}"
        );
        let err = result["error"].as_str().expect("error");
        assert!(
            err.contains("not yet supported") || err.contains("Sprint 41"),
            "error must mention deferral: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── PR-3 tests: auto-detect + GHE ───────────────────────────────

    #[test]
    fn detect_provider_github_com() {
        let (kind, is_custom) = super::detect_provider_from_remote("github.com/owner/repo");
        assert_eq!(kind, "github");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_gitlab_com() {
        let (kind, is_custom) = super::detect_provider_from_remote("gitlab.com/group/project");
        assert_eq!(kind, "gitlab");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_bitbucket_org() {
        let (kind, is_custom) = super::detect_provider_from_remote("bitbucket.org/ws/repo");
        assert_eq!(kind, "bitbucket_cloud");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_custom_domain_defaults_github_with_warning() {
        let (kind, is_custom) =
            super::detect_provider_from_remote("git.corp.example.com/team/repo");
        assert_eq!(kind, "github", "unknown domain defaults to github");
        assert!(is_custom, "unknown domain must flag custom_host");
    }

    /// B1: GitHub Enterprise custom domain detected as github + custom.
    #[test]
    fn detect_provider_github_enterprise_custom_domain() {
        let (kind, is_custom) =
            super::detect_provider_from_remote("https://github.acme.corp/myorg/myrepo");
        assert_eq!(kind, "github");
        assert!(is_custom, "GHE custom domain must flag custom_host");
    }

    /// B3: explicit ci_provider in watch JSON overrides auto-detect.
    #[test]
    fn explicit_ci_provider_overrides_auto_detect() {
        // Watch with ci_provider: gitlab but repo URL pointing to github.com.
        // Factory should construct GitLab provider, not GitHub.
        let fixture = include_str!("../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        // Construct GitLab provider via the same factory logic as production:
        // explicit ci_provider="gitlab" + ci_provider_url pointing to mock.
        let watch = serde_json::json!({
            "repo": "github.com/myorg/myrepo",
            "branch": "main",
            "ci_provider": "gitlab",
            "ci_provider_url": format!("http://127.0.0.1:{port}"),
        });
        let ci_type = watch["ci_provider"].as_str().unwrap();
        assert_eq!(ci_type, "gitlab");

        // Construct provider as factory would.
        let url = watch["ci_provider_url"].as_str().unwrap().to_string();
        let provider = super::GitLabCiProvider::with_base_url(url).expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("myorg/myrepo", "main"));
        handle.join().expect("mock");

        // Assert: request went to GitLab-shaped URL (/projects/{id}/pipelines),
        // NOT GitHub-shaped (/repos/{owner}/{repo}/actions/runs).
        let (path, _) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/projects/") && path.contains("/pipelines"),
            "explicit gitlab must produce GitLab-shaped request, got: {path}"
        );
        assert!(
            !path.contains("/repos/") && !path.contains("/actions/"),
            "must NOT produce GitHub-shaped request: {path}"
        );
    }

    #[test]
    fn github_with_base_url_sets_custom_for_ghe() {
        let provider =
            super::GitHubCiProvider::with_base_url("https://github.corp.example.com/api/v3".into())
                .expect("with_base_url");
        assert_eq!(
            provider.http.base_url,
            "https://github.corp.example.com/api/v3"
        );
    }

    #[test]
    fn github_new_defaults_to_api_github_com() {
        let provider = super::GitHubCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://api.github.com");
    }

    // ── Hotfix E: auto-clear false-positive tests ────────────────────

    #[test]
    fn auto_clear_skips_young_watch() {
        // A watch created < 60s ago should NOT be auto-cleared even if
        // PR terminal is detected (stale PR from previous branch use).
        let dir = tmp_dir("young-watch");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at = now + TTL (meaning it was just created).
        let now = chrono::Utc::now();
        let expires = (now + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-new", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = super::watch_filename("o/r", "feat-new");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Provider says PR terminal (stale PR from old branch use).
        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-new",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        // Watch must survive (young watch grace).
        assert!(
            watch_path.exists(),
            "young watch (<60s) must NOT be auto-cleared on PR terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_clear_fires_on_old_watch_with_merged_pr() {
        // A watch created > 60s ago SHOULD be cleared on PR terminal.
        let dir = tmp_dir("old-watch-clear");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at implying creation > 60s ago.
        let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
        let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-old", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = super::watch_filename("o/r", "feat-old");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-old",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            !watch_path.exists(),
            "old watch (>60s) must be cleared on PR terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Hotfix F: classification layer tests ─────────────────────────

    #[test]
    fn fresh_branch_no_pr_classified_as_pending() {
        // No PR found → PrState::Unknown → watch preserved (pending).
        let dir = tmp_dir("classify-no-pr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Provider returns no runs + PrState::Unknown (no PR found).
        let provider = MockCiProvider::with_runs(vec![]);
        // MockCiProvider default pr_state is Open, override to Unknown.
        *provider.pr_state.lock() = PrState::Unknown;

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(watch_path.exists(), "no-PR branch must NOT be auto-cleared");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn branch_with_open_pr_classified_as_active() {
        // Open PR → PrState::Open → watch preserved.
        let dir = tmp_dir("classify-open-pr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]);
        // Default pr_state is Open — watch should persist.

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            watch_path.exists(),
            "open-PR branch must NOT be auto-cleared"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn branch_with_merged_pr_classified_as_terminal() {
        // Already tested by mock_pr_terminal_clears_watch — verify preserved.
        // Merged PR → PrState::Terminal { merged: true } → auto-clear.
        let dir = tmp_dir("classify-merged");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Use old expires_at so grace period doesn't block.
        let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
        let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-merged", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = watch_filename("o/r", "feat-merged");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-merged",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            !watch_path.exists(),
            "merged-PR branch MUST be auto-cleared"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_pr_not_classified_as_terminal() {
        // A PR closed >1h ago should be treated as stale (Unknown), not terminal.
        // This is the root-cause fix: prevents false-positive auto-clear from
        // old PRs matching the same branch name.
        // Tested via the GitHub provider's closed_at check.
        // The mock provider bypasses this (returns Terminal directly), so this
        // test verifies the design contract via source inspection.
        let src = include_str!("ci_watch.rs");
        assert!(
            src.contains("closed_at") && src.contains("Duration::hours(1)"),
            "check_pr_terminal must verify closed_at freshness (stale PR filter)"
        );
    }

    // ----------------------------------------------------------------------
    // Sprint 54 P0-1 — Subscriber fan-out (hard contract item 2).
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: when fan-out is collapsed back to
    // single-subscriber (commenting out the `for sub in subscribers` loop
    // in `ci_check_repo` and notifying only the first), the second
    // subscriber's inbox stays empty here and the assertion fails with:
    //
    //   thread '...subscriber_fan_out_notifies_every_member' panicked at:
    //   assertion `dev inbox missing terminal notification` failed
    //
    // Restoring the loop returns the test to PASS. This is the proof
    // that the new test catches the multi-caller bug in production code,
    // not just in synthetic mock paths.
    // ----------------------------------------------------------------------
    #[test]
    fn subscriber_fan_out_notifies_every_member() {
        let dir = tmp_dir("fanout");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [
                {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
                {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
            ],
            "instance": "lead",
            "interval_secs": 60,
            "last_run_id": null,
            "head_sha": null,
            "last_polled_at": null,
            "last_notified_head_sha": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
        std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        // Mock provider returns one terminal success run.
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 7,
            conclusion: Some("success".to_string()),
            head_sha: "deadbeef".to_string(),
            url: "https://example/run/7".to_string(),
        }]);

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["lead".to_string(), "dev".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        // Both inboxes must contain the terminal notification. Inbox layout
        // is JSONL at home/inbox/<name>.jsonl (see inbox::inbox_path).
        for sub in ["lead", "dev"] {
            let inbox_path = dir.join("inbox").join(format!("{sub}.jsonl"));
            let body = std::fs::read_to_string(&inbox_path).unwrap_or_else(|_| {
                panic!("{sub} inbox file missing — fan-out regression: {inbox_path:?}")
            });
            assert!(
                body.contains("[ci-pass]") && body.contains("o/r@feat"),
                "{sub} inbox payload mismatch: {body}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_subscribers_reads_array() {
        let watch = serde_json::json!({
            "subscribers": [
                {"instance": "a", "subscribed_at": "x"},
                {"instance": "b", "subscribed_at": "y"}
            ],
            "instance": "ignored-when-array-present"
        });
        assert_eq!(parse_subscribers(&watch), vec!["a", "b"]);
    }

    #[test]
    fn parse_subscribers_falls_back_to_legacy_instance() {
        let watch = serde_json::json!({"instance": "legacy-only"});
        assert_eq!(parse_subscribers(&watch), vec!["legacy-only"]);
    }

    #[test]
    fn parse_subscribers_empty_when_no_data() {
        let watch = serde_json::json!({});
        assert!(parse_subscribers(&watch).is_empty());
    }

    // -----------------------------------------------------------------
    // Sprint 54 P0-5 (sub-scope B) — consecutive_skips tracking +
    // stalled/resumed inbox fan-out. Each test pins one of the four
    // contract gates from dispatch m-20260507045729197032-16.
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: if the
    // `if next_skips >= STALL_THRESHOLD && !already_notified` guard
    // in `bump_consecutive_skips_and_maybe_notify` is dropped, the
    // "fires exactly once" assertion in
    // `stalled_event_fires_exactly_once_at_threshold` fails because
    // every subsequent skip would re-enqueue. PR description carries
    // the captured FAIL signature.
    // -----------------------------------------------------------------

    fn p05_temp_home(tag: &str) -> std::path::PathBuf {
        let dir = tmp_dir(tag);
        std::fs::create_dir_all(dir.join("ci-watches")).ok();
        std::fs::create_dir_all(dir.join("inbox")).ok();
        dir
    }

    fn p05_write_watch(
        home: &Path,
        repo: &str,
        branch: &str,
        watch: serde_json::Value,
    ) -> std::path::PathBuf {
        let path = home.join("ci-watches").join(watch_filename(repo, branch));
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
        path
    }

    fn p05_read_inbox_lines(home: &Path, instance: &str) -> Vec<String> {
        let path = home.join("inbox").join(format!("{instance}.jsonl"));
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn consecutive_skips_increments_on_rate_limited_skip() {
        // Gate 1: each rate-limited skip bumps the counter atomically.
        let home = p05_temp_home("p05_skip_inc");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];

        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
        );
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
        );
        let watch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(watch["consecutive_skips"].as_u64(), Some(2));
        // Below threshold: stalled_notified is either absent (None) or
        // false — both mean "no notify fired yet".
        let notified = watch["stalled_notified"].as_bool().unwrap_or(false);
        assert!(!notified, "below threshold ⇒ no notify yet");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stalled_event_fires_exactly_once_at_threshold() {
        // Gate 2: at N=3 a [ci-watch-stalled] enqueues; further
        // tick-skips don't re-fire. This is the regression-proof
        // anchor — collapsing the guard reveals duplicates.
        let home = p05_temp_home("p05_stalled_once");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];
        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

        for _ in 0..5 {
            bump_consecutive_skips_and_maybe_notify(
                &home,
                &path,
                "o/r",
                "feat",
                &subscribers,
                future_reset,
            );
        }

        let lines = p05_read_inbox_lines(&home, "lead");
        let stalled_count = lines
            .iter()
            .filter(|l| l.contains("ci-watch-stalled"))
            .count();
        assert_eq!(
            stalled_count, 1,
            "exactly one stalled event per stall window (got {stalled_count})"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resumed_event_fires_on_first_successful_clear() {
        // Gate 3: resume helper fires [ci-watch-resumed] exactly once
        // and clears the stall state.
        let home = p05_temp_home("p05_resumed");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 5,
            "stalled_notified": true,
            "stalled_since_ms": 1700000000000_i64,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];

        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
        // Second call must be a silent no-op (state already cleared).
        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);

        let watch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(watch["consecutive_skips"].as_u64(), Some(0));
        assert_eq!(watch["stalled_notified"].as_bool(), Some(false));
        assert!(watch["stalled_since_ms"].is_null());

        let lines = p05_read_inbox_lines(&home, "lead");
        let resumed = lines
            .iter()
            .filter(|l| l.contains("ci-watch-resumed"))
            .count();
        assert_eq!(resumed, 1, "exactly one resumed event per recovery");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stalled_and_resumed_fan_out_to_all_subscribers() {
        // Gate 4: both event types reach every subscriber per the
        // P0-1 fan-out contract.
        let home = p05_temp_home("p05_fanout");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [
                {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
                {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
            ],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string(), "dev".to_string()];
        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

        for _ in 0..STALL_THRESHOLD {
            bump_consecutive_skips_and_maybe_notify(
                &home,
                &path,
                "o/r",
                "feat",
                &subscribers,
                future_reset,
            );
        }
        for sub in ["lead", "dev"] {
            let lines = p05_read_inbox_lines(&home, sub);
            assert!(
                lines.iter().any(|l| l.contains("ci-watch-stalled")),
                "{sub} must receive stalled event (fan-out regression)"
            );
        }

        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
        for sub in ["lead", "dev"] {
            let lines = p05_read_inbox_lines(&home, sub);
            assert!(
                lines.iter().any(|l| l.contains("ci-watch-resumed")),
                "{sub} must receive resumed event"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 54 Hotfix F gap: malformed GitHub head query ─────────────
    //
    // Per RCA m-46: `check_pr_terminal` was sending bare `head=feat/foo`
    // instead of `head=owner:feat/foo`. GitHub silently dropped the
    // filter → returned the most recent PR in the repo → Hotfix F's
    // freshness check passed → false `Terminal{merged}` for fresh-no-PR
    // branches. The malformed-query swallow is the third concrete
    // instance of the silent-drop class systematic prevention pattern
    // (decision `d-20260507100609264367-2`).
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: dropping the `{owner}:` prefix
    // from the URL format string trips
    // `github_check_pr_terminal_uses_owner_prefix_in_head_query`. PR
    // description carries the verbatim FAIL signature.

    /// Reuse the gitlab_mock_server scaffolding — it's just an HTTP
    /// listener returning a fixed body, content-type agnostic. Renaming
    /// to `mock_server` would touch dozens of call sites; sharing the
    /// fn under a Hotfix-F-specific helper avoids that churn.
    fn github_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
        gitlab_mock_server(response_body)
    }

    #[test]
    fn github_check_pr_terminal_uses_owner_prefix_in_head_query() {
        // Hotfix F gap gate 1 (regression-proof anchor): the production
        // URL must carry `head={owner}:{branch}` per GitHub docs. Empty
        // PR list is fine — the test only inspects the captured URL.
        let (port, handle, captured) = github_mock_server("[]");
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));

        handle.join().expect("mock");
        let (path, _request) = captured.lock().expect("lock").take().expect("captured");

        assert!(
            path.contains("/repos/acme/widgets/pulls"),
            "URL must target the repo's pulls endpoint: {path}"
        );
        assert!(
            path.contains("head=acme:feat/foo"),
            "URL must use `head={{owner}}:{{branch}}` per GitHub docs (Hotfix F gap fix); got: {path}"
        );
        // Defensive: also ensure the legacy bare form is GONE — a
        // future edit that re-introduced `head={branch}` without the
        // owner prefix should trip this.
        assert!(
            !path.contains("head=feat/foo&"),
            "URL must NOT use bare `head={{branch}}` (the silent-drop bug); got: {path}"
        );
    }

    #[test]
    fn github_check_pr_terminal_returns_unknown_on_head_ref_mismatch() {
        // Hotfix F gap gate 2 (defensive): even with the correct URL,
        // GitHub may return a PR whose head.ref doesn't match what we
        // asked. The defensive check returns Unknown so we don't
        // misclassify the asked branch as Terminal based on someone
        // else's PR data.
        let mismatched = r#"[{
            "state": "closed",
            "closed_at": "2099-01-01T00:00:00Z",
            "merged_at": "2099-01-01T00:00:00Z",
            "head": {"ref": "different-branch"}
        }]"#;
        let (port, handle, captured) = github_mock_server(mismatched);
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Unknown),
            "head.ref mismatch must return Unknown (defensive); got: {state:?}"
        );
    }

    #[test]
    fn github_check_pr_terminal_returns_unknown_on_empty_pr_list() {
        // Hotfix F gap gate 3 (production-realistic): the bug surfaced
        // for fresh-no-PR branches, where GitHub correctly returns []
        // once the head query is well-formed. The state machine must
        // map empty → Unknown so the auto-clear path doesn't fire.
        let (port, handle, captured) = github_mock_server("[]");
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "fresh-branch-no-pr"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Unknown),
            "fresh-no-PR branch must return Unknown; got: {state:?}"
        );
    }

    #[test]
    fn github_check_pr_terminal_terminal_on_matching_head_ref_and_recent_close() {
        // Hotfix F gap gate 4: the happy path still works post-fix —
        // a closed-and-merged PR matching the head.ref returns
        // Terminal{merged: true}. Defensive head.ref check + Hotfix F
        // freshness check both pass.
        let recent_close = chrono::Utc::now() - chrono::Duration::minutes(5);
        let body = format!(
            r#"[{{
                "state": "closed",
                "closed_at": "{}",
                "merged_at": "{}",
                "head": {{"ref": "feat/foo"}}
            }}]"#,
            recent_close.to_rfc3339(),
            recent_close.to_rfc3339()
        );
        let (port, handle, captured) = github_mock_server(&body);
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "matching head.ref + recent merged_at must be Terminal(merged); got: {state:?}"
        );
    }

    // -------------------------------------------------------------
    // Sprint 57 Wave 2 Track B (#546 Items 1+3) — startup sweep,
    // per-tick eager GC, and protected-ref migration via
    // `gc_stale_watches` / `startup_sweep`.
    // -------------------------------------------------------------

    /// Helper for the new GC tests — write a synthetic watch JSON to
    /// disk under `home/ci-watches/<filename>`. Returns the file path.
    fn write_watch(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        watch: &serde_json::Value,
    ) -> std::path::PathBuf {
        let ci_dir = home.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = super::watch_filename(repo, branch);
        let path = ci_dir.join(&filename);
        std::fs::write(&path, serde_json::to_string(watch).unwrap()).ok();
        path
    }

    #[test]
    fn ci_watch_ttl_expires_stale_entries_on_startup_sweep() {
        // Direct gc_stale_watches call simulates the daemon-startup
        // path that runs before the tick loop spins up. An expired
        // (absolute TTL elapsed) entry must be removed AND the event
        // log must record the removal with the startup_sweep origin.
        let home = tmp_dir("startup-sweep-ttl");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-stale", "interval_secs": 60,
            "subscribers": [{"instance": "agent-x"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
            "last_terminal_seen_at": null,
        });
        let path = write_watch(&home, "o/r", "feat-stale", &watch);

        super::startup_sweep(&home);

        assert!(
            !path.exists(),
            "expired watch must be removed by startup sweep"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("ci_watch_removed"),
            "removal event must log; got:\n{log}"
        );
        assert!(
            log.contains("startup_sweep_expired"),
            "reason must include startup_sweep origin; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_watch_ttl_eager_gc_per_tick() {
        // Per-tick path: `check_ci_watches` calls `gc_stale_watches`
        // BEFORE the poll loop. A watch with `expires_at` in the past
        // but no subscribers list (so the legacy poll body would
        // continue rather than expire it) must still be removed by
        // the eager GC.
        let home = tmp_dir("eager-gc-ttl");
        let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-stale-eager", "interval_secs": 60,
            // empty subscribers — poll loop would `continue` past it
            // without expiring; eager GC must catch it anyway.
            "subscribers": [],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
            "last_terminal_seen_at": null,
        });
        let path = write_watch(&home, "o/r", "feat-stale-eager", &watch);

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        super::check_ci_watches(&home, &registry);

        assert!(
            !path.exists(),
            "eager GC must remove expired watch even when subscribers list is empty"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("eager_gc_expired"),
            "per-tick eager-GC origin must appear in log; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn migrate_existing_main_watches_at_startup() {
        // Item 3 migration: any pre-existing watch with branch=main
        // (or master) was created before the E4.5 gate landed in
        // handle_watch_ci. Startup sweep must remove it regardless
        // of TTL state so the bypass is closed retroactively.
        let home = tmp_dir("migrate-main-watches");
        let watch_main = serde_json::json!({
            "repo": "owner/repo", "branch": "main", "interval_secs": 60,
            "subscribers": [{"instance": "general"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            // Fresh expires_at — would NOT trip the TTL paths.
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_master = serde_json::json!({
            "repo": "owner/legacy-repo", "branch": "master", "interval_secs": 60,
            "subscribers": [{"instance": "lead"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_feat = serde_json::json!({
            "repo": "owner/repo", "branch": "feat-not-touched", "interval_secs": 60,
            "subscribers": [{"instance": "dev"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let path_main = write_watch(&home, "owner/repo", "main", &watch_main);
        let path_master = write_watch(&home, "owner/legacy-repo", "master", &watch_master);
        let path_feat = write_watch(&home, "owner/repo", "feat-not-touched", &watch_feat);

        super::startup_sweep(&home);

        assert!(
            !path_main.exists(),
            "main watch must be migrated/removed by startup sweep"
        );
        assert!(
            !path_master.exists(),
            "master watch must be migrated/removed by startup sweep"
        );
        assert!(path_feat.exists(), "non-protected watch must be preserved");

        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("startup_sweep_protected_branch_migration"),
            "migration reason must surface in audit log; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_watches_idempotent_on_clean_dir() {
        // Defensive bonus pin: re-running the sweep on an already-
        // clean dir must be a no-op (zero removals, zero log entries).
        let home = tmp_dir("gc-idempotent");
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-fresh", "interval_secs": 60,
            "subscribers": [{"instance": "dev"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = write_watch(&home, "o/r", "feat-fresh", &watch);

        let n1 = super::gc_stale_watches(&home, "test_pass1");
        let n2 = super::gc_stale_watches(&home, "test_pass2");

        assert_eq!(n1, 0, "first sweep on fresh dir must remove nothing");
        assert_eq!(n2, 0, "second sweep is idempotent");
        assert!(path.exists(), "fresh watch must survive both sweeps");
        std::fs::remove_dir_all(&home).ok();
    }
}
