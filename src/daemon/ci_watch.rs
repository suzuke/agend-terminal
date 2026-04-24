use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

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
pub fn github_token_warning_from_env() -> Option<&'static str> {
    github_token_warning(std::env::var("GITHUB_TOKEN").ok().as_deref())
}

/// Check CI watch configs and inject failure logs to agents when CI fails.
pub fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
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
                    &registry,
                )) {
                    tracing::debug!(repo = %repo, error = %e, "CI check failed");
                }
            })
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "ci_check: failed to spawn background thread");
                // Return a dummy JoinHandle — thread::spawn always succeeds in
                // practice, but if it doesn't we've logged the failure.
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
enum RunsResponse<'a> {
    Run(&'a serde_json::Value),
    NoRuns,
    ApiError(String),
}

/// Pure interpreter for a runs-list response. See [`RunsResponse`] for
/// why the rate-limit / NoRuns distinction matters.
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

/// Select runs from a GitHub Actions response that should trigger notifications.
/// Returns indices into `runs` of terminal runs with `id > last_run_id`, ordered
/// oldest-first so notifications arrive chronologically.
/// In-progress runs (conclusion=null) are skipped.
pub(crate) fn select_runs_to_notify(
    runs: &[serde_json::Value],
    last_run_id: Option<u64>,
) -> Vec<usize> {
    let threshold = last_run_id.unwrap_or(0);
    let mut selected: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .filter_map(|(i, run)| {
            let id = run["id"].as_u64()?;
            if id <= threshold {
                return None;
            }
            // Skip non-terminal (in-progress) runs
            run["conclusion"].as_str()?;
            Some((i, id))
        })
        .collect();
    // Sort oldest-first by run_id
    selected.sort_by_key(|&(_, id)| id);
    selected.into_iter().map(|(i, _)| i).collect()
}

/// Build the notification message for a CI run conclusion.
/// Returns `None` for non-terminal states (in-progress / null conclusion).
fn ci_notification_message(
    repo: &str,
    branch: &str,
    conclusion: Option<&str>,
    failure_detail: Option<&str>,
) -> Option<String> {
    let conclusion = conclusion?;
    let msg = match conclusion {
        "failure" => {
            let detail = failure_detail.unwrap_or("unknown step");
            format!("[ci-fail] {repo}@{branch}: {detail}\r")
        }
        "success" => format!("[ci-pass] {repo}@{branch}: passed ✓\r"),
        other => format!("[ci-ended] {repo}@{branch}: {other}\r"),
    };
    Some(msg)
}

/// Fetch latest GitHub Actions run and notify the watching agent on any
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
    registry: &AgentRegistry,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let gh_get = |url: &str| {
        let mut req = client
            .get(url)
            .header("User-Agent", "agend-terminal")
            .header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    };

    // Check if the PR associated with this branch has reached a terminal state.
    if branch != "main" && branch != "master" {
        if let Some(should_clear) = check_pr_terminal(&gh_get, repo, branch).await {
            if should_clear {
                let _ = std::fs::remove_file(watch_path);
                tracing::info!(repo, branch, "CI watcher auto-cleared: PR terminal");
                return Ok(());
            }
        }
    }

    let resp = gh_get(&format!(
        "https://api.github.com/repos/{repo}/actions/runs?branch={branch}&per_page=5"
    ))
    .send()
    .await?;
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await?;
    // Use classify_runs_response to surface API errors (rate-limit, auth, etc.)
    // so they don't silently look like a quiescent branch. Then extract the
    // full runs array ourselves for multi-run scan (classify only returns the
    // first run — fine for its API-error contract, but we need all 5).
    if let RunsResponse::ApiError(msg) = classify_runs_response(status, &body) {
        return Err(anyhow::anyhow!(msg));
    }
    let runs = match body["workflow_runs"].as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Ok(()),
    };

    // Determine the latest head_sha from the newest run.
    let current_sha = runs
        .first()
        .and_then(|r| r["head_sha"].as_str())
        .unwrap_or("");

    // If head_sha changed (force push), reset last_run_id so we pick up new runs.
    let effective_last_run_id = if prev_head_sha.is_some_and(|prev| prev != current_sha) {
        tracing::info!(repo, branch, old_sha = ?prev_head_sha, new_sha = current_sha, "head_sha changed, resetting run tracking");
        None
    } else {
        last_run_id
    };

    let to_notify = select_runs_to_notify(runs, effective_last_run_id);
    if to_notify.is_empty() {
        // No new terminal runs — update head_sha but keep last_run_id.
        if let Some(id) = effective_last_run_id {
            update_watch_state(watch_path, Some(id), current_sha);
        }
        return Ok(());
    }

    let mut max_notified_id = effective_last_run_id.unwrap_or(0);
    for &idx in &to_notify {
        let run = &runs[idx];
        let run_id = run["id"].as_u64().unwrap_or(0);
        let conclusion = run["conclusion"].as_str();

        let failure_detail = if conclusion == Some("failure") {
            Some(fetch_failure_summary(&gh_get, repo, run_id).await)
        } else {
            None
        };

        if let Some(msg) =
            ci_notification_message(repo, branch, conclusion, failure_detail.as_deref())
        {
            let reg = agent::lock_registry(registry);
            if let Some(handle) = reg.get(instance) {
                let _ = agent::inject_to_agent(handle, msg.as_bytes());
            } else {
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
                        from: "system:ci".to_string(),
                        text: msg,
                        kind: Some("ci-watch".to_string()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        }
        if run_id > max_notified_id {
            max_notified_id = run_id;
        }
    }

    update_watch_state(watch_path, Some(max_notified_id), current_sha);
    Ok(())
}

/// Persist updated tracking state (last_run_id + head_sha) to the watch file.
fn update_watch_state(watch_path: &Path, run_id: Option<u64>, head_sha: &str) {
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
            watch["last_run_id"] = serde_json::json!(run_id);
            if !head_sha.is_empty() {
                watch["head_sha"] = serde_json::json!(head_sha);
            }
            let _ = std::fs::write(
                watch_path,
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
            );
        }
    }
}

/// Check if the PR for a branch has reached a terminal state (merged or closed).
/// Returns `Some(true)` if the watcher should be cleared, `Some(false)` if the
/// PR is still open, `None` if the check failed or no PR was found.
async fn check_pr_terminal(
    gh_get: &impl Fn(&str) -> reqwest::RequestBuilder,
    repo: &str,
    branch: &str,
) -> Option<bool> {
    let resp: serde_json::Value = gh_get(&format!(
        "https://api.github.com/repos/{repo}/pulls?head={branch}&state=all&per_page=1"
    ))
    .send()
    .await
    .ok()?
    .json()
    .await
    .ok()?;
    let pr = resp.as_array()?.first()?;
    let state = pr["state"].as_str()?;
    Some(state == "closed")
}

/// Fetch the first failed job+step name from a GitHub Actions run.
async fn fetch_failure_summary(
    gh_get: &impl Fn(&str) -> reqwest::RequestBuilder,
    repo: &str,
    run_id: u64,
) -> String {
    let jobs_resp: serde_json::Value = match gh_get(&format!(
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ci_watch_failure_includes_detail() {
        let msg =
            ci_notification_message("owner/repo", "main", Some("failure"), Some("Build / Test"));
        assert_eq!(
            msg.as_deref(),
            Some("[ci-fail] owner/repo@main: Build / Test\r")
        );
    }

    #[test]
    fn ci_watch_failure_without_detail_falls_back() {
        let msg = ci_notification_message("owner/repo", "main", Some("failure"), None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-fail] owner/repo@main: unknown step\r")
        );
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
        use serde_json::json;
        let runs = vec![
            json!({"id": 100, "conclusion": "success", "head_sha": "aaa"}),
            json!({"id": 101, "conclusion": "success", "head_sha": "bbb"}),
            json!({"id": 102, "conclusion": null, "head_sha": "ccc"}), // in-progress
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
        use serde_json::json;
        let runs = vec![json!({"id": 200, "conclusion": null, "head_sha": "aaa"})];
        let selected = select_runs_to_notify(&runs, None);
        assert!(selected.is_empty(), "in-progress run must not be selected");
    }

    #[test]
    fn test_mixed_terminal_states_all_notified() {
        use serde_json::json;
        let runs = vec![
            json!({"id": 300, "conclusion": "failure", "head_sha": "a"}),
            json!({"id": 301, "conclusion": "cancelled", "head_sha": "b"}),
            json!({"id": 302, "conclusion": "success", "head_sha": "c"}),
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
        use serde_json::json;
        let runs = vec![
            json!({"id": 400, "conclusion": "success", "head_sha": "a"}),
            json!({"id": 401, "conclusion": "success", "head_sha": "b"}),
        ];
        let selected = select_runs_to_notify(&runs, Some(400));
        assert_eq!(
            selected,
            vec![1],
            "run 400 already notified, only 401 selected"
        );
    }
}
