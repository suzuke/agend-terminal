use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

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

        // Throttle via mtime
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|age| age.as_secs() < interval)
                .unwrap_or(false)
            {
                continue;
            }
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&watch).unwrap_or_default(),
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
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    instance: &str,
    last_run_id: Option<u64>,
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

    // Skip duplicate notifications for the same run.
    if Some(run_id) == last_run_id {
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
                from: "system:ci".to_string(),
                text: msg,
                kind: Some("ci-watch".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    // Update last_run_id for any terminal state to prevent re-notification.
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
            watch["last_run_id"] = serde_json::json!(run_id);
            let _ = std::fs::write(
                watch_path,
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
            );
        }
    }
    Ok(())
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
}
