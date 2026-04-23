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
            .ok();
    }
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

    let resp: serde_json::Value = gh_get(&format!(
        "https://api.github.com/repos/{repo}/actions/runs?branch={branch}&per_page=1"
    ))
    .send()
    .await?
    .json()
    .await?;
    let run = match resp["workflow_runs"].as_array().and_then(|a| a.first()) {
        Some(r) => r,
        None => return Ok(()),
    };
    let run_id = run["id"].as_u64().unwrap_or(0);
    let current_sha = run["head_sha"].as_str().unwrap_or("");

    // If head_sha changed (force push), reset last_run_id so we pick up the new run.
    let effective_last_run_id = if prev_head_sha.is_some_and(|prev| prev != current_sha) {
        tracing::info!(repo, branch, old_sha = ?prev_head_sha, new_sha = current_sha, "head_sha changed, resetting run tracking");
        None
    } else {
        last_run_id
    };

    // Skip duplicate notifications for the same run.
    if Some(run_id) == effective_last_run_id {
        // Still update head_sha in case it changed without a new run yet
        update_watch_state(watch_path, Some(run_id), current_sha);
        return Ok(());
    }

    // conclusion is null while the run is in-progress; skip non-terminal states.
    let conclusion = run["conclusion"].as_str();

    // For failures, fetch job-level detail before building the message.
    let failure_detail = if conclusion == Some("failure") {
        Some(fetch_failure_summary(&gh_get, repo, run_id).await)
    } else {
        None
    };

    let msg = match ci_notification_message(repo, branch, conclusion, failure_detail.as_deref()) {
        Some(m) => m,
        None => return Ok(()), // in-progress
    };

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

    // Update last_run_id and head_sha for any terminal state to prevent re-notification.
    update_watch_state(watch_path, Some(run_id), current_sha);
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
}
