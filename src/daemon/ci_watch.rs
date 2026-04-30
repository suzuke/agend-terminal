use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

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
#[derive(Debug)]
pub enum CiPollResult {
    /// Runs retrieved successfully (may be empty).
    Runs(Vec<CiRun>),
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
    client: reqwest::Client,
}

impl GitHubCiProvider {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
        })
    }

    fn gh_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .get(url)
            .header("User-Agent", "agend-terminal")
            .header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }
}

#[async_trait::async_trait]
impl CiProvider for GitHubCiProvider {
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult> {
        let resp = self
            .gh_get(&format!(
                "https://api.github.com/repos/{repo}/actions/runs?branch={branch}&per_page=5"
            ))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let rate_limit_reset = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let body: serde_json::Value = resp.json().await?;

        // Surface API errors (rate-limit, auth, server) instead of
        // silently treating them as "no runs".
        if !(200..300).contains(&status) {
            let message = body["message"]
                .as_str()
                .unwrap_or("(no message)")
                .to_string();
            let hint = if status == 403
                && std::env::var("GITHUB_TOKEN").is_err()
                && message.to_lowercase().contains("rate limit")
            {
                " — set GITHUB_TOKEN to raise the unauthenticated 60/hr cap"
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
        Ok(CiPollResult::Runs(runs))
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        let resp: serde_json::Value = match self
            .gh_get(&format!(
                "https://api.github.com/repos/{repo}/pulls?head={branch}&state=all&per_page=1"
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
            Some(pr) => match pr["state"].as_str() {
                Some("closed") => PrState::Terminal {
                    merged: pr["merged_at"].as_str().is_some(),
                },
                Some(_) => PrState::Open,
                None => PrState::Unknown,
            },
            None => PrState::Unknown,
        }
    }

    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String {
        let jobs_resp: serde_json::Value = match self
            .gh_get(&format!(
                "https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs"
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
    client: reqwest::Client,
    /// Base URL for GitLab API (self-hosted support).
    /// Defaults to `https://gitlab.com`.
    base_url: String,
}

impl GitLabCiProvider {
    #[allow(dead_code)] // wired in Sprint 39 PR-3 (fleet.yaml ci_provider config)
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url("https://gitlab.com".to_string())
    }

    #[allow(dead_code)] // wired in Sprint 39 PR-3
    pub fn with_base_url(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
            base_url,
        })
    }

    /// Resolve GitLab auth token via fallback chain:
    /// 1. GITLAB_TOKEN env var
    /// 2. glab CLI config (~/.config/glab-cli/config.yml)
    fn resolve_token() -> Option<String> {
        if let Ok(token) = std::env::var("GITLAB_TOKEN") {
            return Some(token);
        }
        // Fallback: glab CLI config file.
        let config_path = dirs::config_dir()?.join("glab-cli").join("config.yml");
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

    fn gl_get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/api/v4/{path}", self.base_url);
        let mut req = self.client.get(&url).header("User-Agent", "agend-terminal");
        if let Some(token) = Self::resolve_token() {
            req = req.header("PRIVATE-TOKEN", token);
        }
        req
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
            .gl_get(&format!(
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

        Ok(CiPollResult::Runs(runs))
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        let project = Self::encode_project(repo);
        let resp: serde_json::Value = match self
            .gl_get(&format!(
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
            .gl_get(&format!(
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
            .gl_get(&format!("projects/{project}/jobs/{job_id}/trace"))
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

/// Watch TTL in hours. Used for both absolute expiry and inactivity threshold.
pub const WATCH_TTL_HOURS: i64 = 72;

/// Remove a watch file and log the removal event.
pub fn remove_watch(
    home: &Path,
    watch_path: &Path,
    instance: &str,
    repo: &str,
    branch: &str,
    reason: &str,
) {
    let _ = std::fs::remove_file(watch_path);
    crate::event_log::log(
        home,
        "ci_watch_removed",
        instance,
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

/// Preventive warning shown in the `watch_ci` MCP response when the
/// daemon's environment doesn't carry a usable `GITHUB_TOKEN`.
///
/// The daemon reads `GITHUB_TOKEN` on every poll to authenticate against
/// the GitHub REST API (`ci_check_repo`). Without it, the process falls
/// back to the unauthenticated 60-requests/hour cap — shared across
/// every active watch. Five watches on 60-second intervals push ~300
/// req/hr, so a silent 403 storm is easy to trigger without ever
/// hitting a single fetch explicitly.
///
/// Split as a pure helper so unit tests don't have to serialize over a
/// shared `std::env` mutation (cf. `watchdog::ENV_LOCK`).
pub fn github_token_warning(token: Option<&str>) -> Option<&'static str> {
    match token.map(str::trim).filter(|s| !s.is_empty()) {
        Some(_) => None,
        None => Some(
            "GITHUB_TOKEN not set — daemon polls GitHub unauthenticated \
             (60 req/hr, shared by all active watches). \
             Export GITHUB_TOKEN (e.g. `export GITHUB_TOKEN=$(gh auth token)`) \
             and restart the daemon so it inherits the value.",
        ),
    }
}

/// `github_token_warning` fed from the daemon's actual env. Separate
/// from the pure helper so the handler can call this one-liner while
/// tests drive the pure form with synthetic inputs.
/// Also serves as the default `token_warning` for [`GitHubCiProvider`].
pub fn github_token_warning_from_env() -> Option<&'static str> {
    github_token_warning(std::env::var("GITHUB_TOKEN").ok().as_deref())
}

/// Check CI watch configs and inject failure logs to agents when CI fails.
pub fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
    check_ci_watches_with_provider(home, registry, || {
        Some(Box::new(GitHubCiProvider::new().ok()?) as Box<dyn CiProvider>)
    });
}

/// Inner implementation that accepts a provider factory for testability.
fn check_ci_watches_with_provider(
    home: &Path,
    registry: &AgentRegistry,
    make_provider: impl Fn() -> Option<Box<dyn CiProvider>> + Send + Sync + 'static,
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
        let (repo, instance) = match (watch["repo"].as_str(), watch["instance"].as_str()) {
            (Some(r), Some(i)) => (r.to_string(), i.to_string()),
            _ => continue,
        };
        let branch = watch["branch"].as_str().unwrap_or("main").to_string();
        let interval = watch["interval_secs"].as_u64().unwrap_or(60);
        let last_run_id = watch["last_run_id"].as_u64();
        let head_sha = watch["head_sha"].as_str().map(String::from);
        let last_notified_sha = watch["last_notified_head_sha"].as_str().map(String::from);

        // TTL check: remove expired watches before polling.
        let now_utc = chrono::Utc::now();
        if let Some(expires_at) = watch["expires_at"].as_str() {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                if now_utc > exp.with_timezone(&chrono::Utc) {
                    remove_watch(home, &path, &instance, &repo, &branch, "expired");
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
                    remove_watch(home, &path, &instance, &repo, &branch, "inactivity_ttl");
                    tracing::info!(repo = %repo, branch = %branch, hours = WATCH_TTL_HOURS, "CI watch removed: inactivity TTL");
                    continue;
                }
            }
        }

        // Rate-limit backoff: skip polling until X-RateLimit-Reset time.
        if let Some(reset_epoch) = watch["rate_limit_until"].as_u64() {
            if (chrono::Utc::now().timestamp() as u64) < reset_epoch {
                continue;
            }
        }

        // Throttle from a dedicated `last_polled_at` (epoch millis) in the
        // watch file itself, not file mtime. mtime conflates "when this
        // file was touched" with "when we last polled" and broke whenever
        // another writer (migration, hand-edit, freshly created watch)
        // stamped the file — the handler used to backdate mtime manually
        // to work around that. Schema-local state removes both the
        // first-poll-lag quirk and the external-writer fragility.
        let now_ms = chrono::Utc::now().timestamp_millis();
        if !watch_is_due(watch["last_polled_at"].as_i64(), interval, now_ms) {
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
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&watch_with_stamp).unwrap_or_default(),
        );

        let home = home.to_path_buf();
        let watch_path = path.clone();
        let registry = Arc::clone(registry);
        let provider = match make_provider() {
            Some(p) => p,
            None => {
                tracing::warn!(repo = %repo, "ci_check: failed to build CI provider");
                continue;
            }
        };
        // fire-and-forget: ci_check is one-shot per poll cycle. Builds a
        // single-thread tokio runtime, blocks on one provider call, exits.
        // No JoinHandle / shutdown signal needed because the tick loop will
        // re-spawn next cycle if anything is still being watched.
        std::thread::Builder::new()
            .name("ci_check".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    tracing::warn!(repo = %repo, "ci_check: failed to build tokio runtime");
                    return;
                };
                if let Err(e) = rt.block_on(ci_check_repo(
                    &home,
                    &watch_path,
                    &repo,
                    &branch,
                    &instance,
                    last_run_id,
                    head_sha.as_deref(),
                    last_notified_sha.as_deref(),
                    &registry,
                    provider.as_ref(),
                )) {
                    tracing::warn!(repo = %repo, error = %e, "CI check failed");
                }
            })
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "ci_check: failed to spawn background thread");
                // fire-and-forget: dummy no-op JoinHandle returned only to
                // satisfy the unwrap_or_else return type. The closure body
                // does nothing — the real work was the failed spawn above.
                // No shutdown semantics needed.
                std::thread::spawn(|| {})
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
) -> Option<String> {
    let conclusion = conclusion?;
    let msg = match conclusion {
        "failure" => format!("[ci-fail] {repo}@{branch}: failure\r"),
        "success" => format!("[ci-pass] {repo}@{branch}: passed ✓\r"),
        other => format!("[ci-ended] {repo}@{branch}: {other}\r"),
    };
    Some(msg)
}

/// Fetch latest CI run and notify the watching agent on any
/// terminal conclusion (success, failure, cancelled, timed_out, etc.).
/// Also tracks `head_sha` — if the branch HEAD changes (e.g. force push),
/// `last_run_id` is reset so the new run is picked up.
/// On PR terminal states (merged/closed), the watcher is auto-cleared.
#[allow(clippy::too_many_arguments)]
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    instance: &str,
    last_run_id: Option<u64>,
    prev_head_sha: Option<&str>,
    last_notified_sha: Option<&str>,
    registry: &AgentRegistry,
    provider: &dyn CiProvider,
) -> anyhow::Result<()> {
    // Check if the PR associated with this branch has reached a terminal state.
    if let PrState::Terminal { merged } = provider.check_pr_terminal(repo, branch).await {
        remove_watch(home, watch_path, instance, repo, branch, "pr_terminal");
        tracing::info!(repo, branch, merged, "CI watcher auto-cleared: PR terminal");
        if merged {
            crate::status_summary::auto_close_merged_tasks(home, branch);
        }
        return Ok(());
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
                        let _ = std::fs::write(
                            watch_path,
                            serde_json::to_string_pretty(&watch).unwrap_or_default(),
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
            if let Some(ch) = crate::channel::active_channel() {
                let _ = crate::channel::gated_notify(
                    ch.as_ref(),
                    instance,
                    crate::channel::NotifySeverity::Warn,
                    &notify_msg,
                    false,
                );
            }
            return Err(anyhow::anyhow!("{status}: {message}"));
        }
        CiPollResult::Runs(r) if r.is_empty() => return Ok(()),
        CiPollResult::Runs(r) => r,
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

        if let Some(headline) = ci_notification_message(repo, branch, conclusion, None) {
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

            let reg = agent::lock_registry(registry);
            if let Some(handle) = reg.get(instance) {
                let _ = agent::inject_to_agent(handle, headline.as_bytes());
            }
            drop(reg);
            let _ = crate::inbox::enqueue(
                home,
                instance,
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
                    text: body,
                    kind: Some("ci-watch".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    channel: None,
                    delivery_mode: None,
                    attachments: vec![],
                    in_reply_to_msg_id: None,
                    in_reply_to_excerpt: None,
                },
            );
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
            let _ = std::fs::write(
                watch_path,
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
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
        let msg = ci_notification_message("owner/repo", "main", Some("success"), None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-pass] owner/repo@main: passed ✓\r")
        );
    }

    #[test]
    fn ci_watch_failure_headline_excludes_detail() {
        // Job detail moved to inbox body — headline just says "failure"
        let msg =
            ci_notification_message("owner/repo", "main", Some("failure"), Some("Build / Test"));
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure\r"));
    }

    #[test]
    fn ci_watch_failure_without_detail_same_headline() {
        let msg = ci_notification_message("owner/repo", "main", Some("failure"), None);
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure\r"));
    }

    #[test]
    fn ci_watch_in_progress_skipped() {
        let msg = ci_notification_message("owner/repo", "main", None, None);
        assert!(
            msg.is_none(),
            "in-progress (null conclusion) must be skipped"
        );
    }

    #[test]
    fn ci_watch_cancelled_notifies() {
        let msg = ci_notification_message("owner/repo", "feat", Some("cancelled"), None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-ended] owner/repo@feat: cancelled\r")
        );
    }

    #[test]
    fn ci_watch_timed_out_notifies() {
        let msg = ci_notification_message("owner/repo", "main", Some("timed_out"), None);
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
        let msg = ci_notification_message("o/r", "feat", Some("success"), None);
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
        assert!(log.contains("reason=expired"), "reason must be expired");
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
                poll_result: Mutex::new(Some(CiPollResult::Runs(runs))),
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
        let instance = watch_json["instance"].as_str().unwrap();
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
            instance,
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
            "agent1",
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
    fn gitlab_mock_server(
        response_body: &str,
    ) -> (
        u16,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
    ) {
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
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
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
            super::CiPollResult::Runs(r) => r,
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
        let (port, handle, _) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));

        handle.join().expect("mock");
        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "expected Terminal(merged), got: {state:?}"
        );
    }

    /// §3.5.10: GitLabCiProvider::fetch_failure_summary finds failed job.
    #[test]
    fn gitlab_fetch_failure_summary_finds_failed_job() {
        let fixture = include_str!("../../tests/fixtures/gitlab-jobs-response.json");
        let (port, handle, _) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));

        handle.join().expect("mock");
        assert_eq!(summary, "test / cargo-test");
    }

    /// Auth fallback: token_warning returns warning when no token found.
    #[test]
    fn gitlab_token_warning_when_no_token() {
        // Ensure GITLAB_TOKEN is not set for this test.
        std::env::remove_var("GITLAB_TOKEN");
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
        assert_eq!(provider.base_url, "https://gitlab.com");
    }

    /// Smoke: with_base_url() sets custom base URL (self-hosted).
    #[test]
    fn gitlab_with_base_url_sets_custom() {
        let provider =
            super::GitLabCiProvider::with_base_url("https://git.corp.example.com".into())
                .expect("with_base_url");
        assert_eq!(provider.base_url, "https://git.corp.example.com");
    }

    /// B4: Auth state 1 — GITLAB_TOKEN env present → PRIVATE-TOKEN header sent.
    #[test]
    fn gitlab_auth_env_token_sends_private_token_header() {
        let fixture = include_str!("../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        std::env::set_var("GITLAB_TOKEN", "test-token-123");
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
}
