use crate::agent_ops::validate_branch;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let source = match args["source"].as_str() {
        Some(s) => s,
        None => return json!({"error": "missing 'source'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    if !validate_branch(branch) {
        return json!({"error": format!("invalid branch name '{branch}'")});
    }
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace('/', "_").replace('~', "")
    ));
    std::fs::create_dir_all(worktree_dir.parent().unwrap_or(home)).ok();
    let source_path = if source.starts_with('/') || source.starts_with('~') {
        source
            .strip_prefix("~/")
            .map(|rest| format!("{}/{rest}", crate::user_home_dir().display()))
            .unwrap_or_else(|| source.to_string())
    } else {
        crate::api::call(home, &json!({"method": crate::api::method::LIST}))
            .ok()
            .and_then(|r| {
                r["result"]["agents"]
                    .as_array()?
                    .iter()
                    .find(|a| a["name"].as_str() == Some(source))
                    .and_then(|a| a["working_directory"].as_str().map(String::from))
            })
            .unwrap_or_else(|| source.to_string())
    };
    match std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &worktree_dir.display().to_string(),
            branch,
        ])
        .current_dir(&source_path)
        .output()
    {
        Ok(o) if o.status.success() => {
            json!({"path": worktree_dir.display().to_string(), "source": source_path, "branch": branch})
        }
        Ok(o) => json!({"error": String::from_utf8_lossy(&o.stderr).to_string()}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub(super) fn handle_release_repo(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return json!({"error": "missing 'path'"}),
    };
    match std::process::Command::new("git")
        .args(["worktree", "remove", "--force", path])
        .output()
    {
        Ok(o) if o.status.success() => json!({"path": path}),
        Ok(o) => {
            let _ = std::fs::remove_dir_all(path);
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(path);
            json!({"path": path})
        }
    }
}

pub(super) fn handle_watch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repo = match args["repo"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repo'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    let interval = args["interval_secs"].as_u64().unwrap_or(60);
    let ci_dir = home.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = json!({
        "repo": repo,
        "branch": branch,
        "interval_secs": interval,
        "instance": instance_name,
        "ci_provider": args["ci_provider"].as_str(),
        "ci_provider_url": args["ci_provider_url"].as_str(),
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let watch_path = ci_dir.join(&filename);
    let _ = std::fs::write(
        &watch_path,
        serde_json::to_string_pretty(&watch).unwrap_or_default(),
    );
    let mut resp = json!({"repo": repo, "watching": true});
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["warning"] = json!(w);
    }
    resp
}

pub(super) fn handle_unwatch_ci(home: &Path, args: &Value) -> Value {
    let repo = match args["repo"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repo'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = home.join("ci-watches").join(&filename);
    let _ = std::fs::remove_file(&path);
    json!({"repo": repo, "watching": false})
}
