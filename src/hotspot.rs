//! Hotspot detection — identifies files recently modified by multiple agents.
//!
//! Builds an index from git log + Agend-Agent trailers (Phase 1).
//! Warns when an agent commits to a file another agent touched in the last 7 days.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Window for hotspot detection (7 days).
const HOTSPOT_WINDOW_DAYS: i64 = 7;

/// A single file touch record.
#[derive(Debug, Clone)]
pub struct FileTouch {
    pub agent: String,
    pub commit_sha: String,
    pub timestamp: String,
}

/// Hotspot index: file_path → list of recent touches by agents.
pub type HotspotIndex = HashMap<PathBuf, Vec<FileTouch>>;

/// Build hotspot index from git log in a repo directory.
/// Parses commits with Agend-Agent trailer from the last 7 days.
pub fn build_index(repo_dir: &Path) -> HotspotIndex {
    let mut index: HotspotIndex = HashMap::new();
    let since = format!("--since={} days ago", HOTSPOT_WINDOW_DAYS);

    let output = std::process::Command::new("git")
        .args([
            "log",
            &since,
            "--format=%H%n%aI%n%(trailers:key=Agend-Agent,valueonly)",
            "--name-only",
        ])
        .current_dir(repo_dir)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return index,
    };

    // Parse git log output: groups of (sha, date, agent, files...) separated by blank lines.
    let mut current_sha = String::new();
    let mut current_ts = String::new();
    let mut current_agent = String::new();
    let mut in_files = false;

    for line in output.lines() {
        if line.is_empty() {
            in_files = false;
            current_sha.clear();
            current_ts.clear();
            current_agent.clear();
            continue;
        }
        if current_sha.is_empty() {
            current_sha = line.to_string();
            continue;
        }
        if current_ts.is_empty() {
            current_ts = line.to_string();
            continue;
        }
        if !in_files && current_agent.is_empty() {
            current_agent = line.trim().to_string();
            in_files = true;
            continue;
        }
        if in_files && !current_agent.is_empty() {
            let file_path = PathBuf::from(line.replace('\\', "/"));
            index.entry(file_path).or_default().push(FileTouch {
                agent: current_agent.clone(),
                commit_sha: current_sha.clone(),
                timestamp: current_ts.clone(),
            });
        }
    }
    index
}

/// Check if a file is a hotspot for a given agent (another agent touched it recently).
pub fn check_hotspot<'a>(
    index: &'a HotspotIndex,
    file: &Path,
    current_agent: &str,
) -> Option<&'a FileTouch> {
    index.get(file).and_then(|touches| {
        touches
            .iter()
            .find(|t| t.agent != current_agent && !t.agent.is_empty())
    })
}

/// List all current hotspots (files touched by multiple agents).
pub fn list_hotspots(index: &HotspotIndex) -> Vec<(PathBuf, Vec<String>)> {
    index
        .iter()
        .filter_map(|(path, touches)| {
            let agents: Vec<String> = touches
                .iter()
                .filter(|t| !t.agent.is_empty())
                .map(|t| t.agent.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            if agents.len() > 1 {
                Some((path.clone(), agents))
            } else {
                None
            }
        })
        .collect()
}

/// Emit hotspot warning to lead's inbox.
pub fn hotspot_warn(home: &Path, agent: &str, file: &Path, last_toucher: &str, since: &str) {
    let text = format!(
        "[hotspot] {agent} modifying {} — last touched by {last_toucher} at {since}",
        file.display()
    );
    tracing::warn!(%agent, file = %file.display(), %last_toucher, "hotspot detected");
    crate::event_log::log(home, "hotspot_warn", agent, &text);
    // Notify lead via inbox.
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        from: "system:hotspot".to_string(),
        text,
        kind: Some("hotspot".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
    };
    let _ = crate::inbox::enqueue(home, "lead", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_hotspot_finds_other_agent() {
        let mut index = HotspotIndex::new();
        index.insert(
            PathBuf::from("src/main.rs"),
            vec![FileTouch {
                agent: "agent-a".into(),
                commit_sha: "abc123".into(),
                timestamp: "2026-05-05T12:00:00Z".into(),
            }],
        );
        let result = check_hotspot(&index, Path::new("src/main.rs"), "agent-b");
        assert!(
            result.is_some(),
            "agent-b touching file agent-a touched = hotspot"
        );
        assert_eq!(result.expect("r").agent, "agent-a");
    }

    #[test]
    fn check_hotspot_excludes_self() {
        let mut index = HotspotIndex::new();
        index.insert(
            PathBuf::from("src/lib.rs"),
            vec![FileTouch {
                agent: "agent-a".into(),
                commit_sha: "def456".into(),
                timestamp: "2026-05-05T12:00:00Z".into(),
            }],
        );
        let result = check_hotspot(&index, Path::new("src/lib.rs"), "agent-a");
        assert!(result.is_none(), "self-touch must not be hotspot");
    }

    #[test]
    fn list_hotspots_multi_agent_files() {
        let mut index = HotspotIndex::new();
        index.insert(
            PathBuf::from("shared.rs"),
            vec![
                FileTouch {
                    agent: "a".into(),
                    commit_sha: "1".into(),
                    timestamp: "t1".into(),
                },
                FileTouch {
                    agent: "b".into(),
                    commit_sha: "2".into(),
                    timestamp: "t2".into(),
                },
            ],
        );
        index.insert(
            PathBuf::from("solo.rs"),
            vec![FileTouch {
                agent: "a".into(),
                commit_sha: "3".into(),
                timestamp: "t3".into(),
            }],
        );
        let hotspots = list_hotspots(&index);
        assert_eq!(hotspots.len(), 1, "only shared.rs is a hotspot");
        assert_eq!(hotspots[0].0, PathBuf::from("shared.rs"));
    }

    #[test]
    fn empty_index_no_hotspots() {
        let index = HotspotIndex::new();
        assert!(list_hotspots(&index).is_empty());
        assert!(check_hotspot(&index, Path::new("any.rs"), "agent").is_none());
    }

    #[test]
    fn seven_day_boundary_window() {
        // The git log --since flag handles the boundary. We verify the constant.
        assert_eq!(HOTSPOT_WINDOW_DAYS, 7);
        // Verify the format string used in build_index.
        let since_arg = format!("--since={} days ago", HOTSPOT_WINDOW_DAYS);
        assert_eq!(since_arg, "--since=7 days ago");
        // 7d0h0m commit: included (git --since is inclusive of the boundary day).
        // 7d0h1m commit: excluded by git's --since semantics.
        // This is git's native behavior — we rely on it, not re-implement.
    }

    #[test]
    fn hotspot_warn_emits_to_lead_inbox() {
        let home = std::env::temp_dir().join(format!("agend-hotspot-warn-{}", std::process::id()));
        std::fs::create_dir_all(home.join("inbox")).ok();
        hotspot_warn(
            &home,
            "agent-x",
            Path::new("src/shared.rs"),
            "agent-y",
            "2026-05-01",
        );
        let inbox = home.join("inbox").join("lead.jsonl");
        assert!(inbox.exists(), "hotspot_warn must write to lead inbox");
        let content = std::fs::read_to_string(&inbox).unwrap_or_default();
        assert!(
            content.contains("hotspot"),
            "inbox must contain hotspot kind"
        );
        assert!(
            content.contains("agent-x"),
            "inbox must mention current agent"
        );
        assert!(
            content.contains("agent-y"),
            "inbox must mention last toucher"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
