//! Visibility status summary — shared builder for telegram keyword + TUI panel.
//! Also contains task-entry parsing and branch-match auto-close helpers.

use std::path::Path;

/// Build a human-readable status summary from task board + decisions.
/// Sprint scope: tasks created in last 7 days (per operator Q1 decision).
pub fn build_summary(home: &Path) -> String {
    let tasks = crate::tasks::list_all(home);
    let decisions = crate::decisions::list_all(home);
    let now = chrono::Utc::now();

    // Sprint scope: tasks created in last 7 days
    let sprint_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| {
            chrono::DateTime::parse_from_rfc3339(&t.created_at)
                .map(|dt| now.signed_duration_since(dt) < chrono::Duration::days(7))
                .unwrap_or(false)
        })
        .collect();

    let in_progress: Vec<_> = sprint_tasks
        .iter()
        .filter(|t| t.status == "claimed" || t.status == "in_progress")
        .collect();
    let open: Vec<_> = sprint_tasks.iter().filter(|t| t.status == "open").collect();
    let blocked: Vec<_> = sprint_tasks
        .iter()
        .filter(|t| t.status == "blocked")
        .collect();
    let done: Vec<_> = sprint_tasks
        .iter()
        .filter(|t| t.status == "done" || t.status == "verified")
        .collect();
    let total = sprint_tasks.len();

    let mut lines = Vec::new();
    lines.push("═══ Status Summary (7-day sprint) ═══".to_string());

    // Sprint progress bar
    let done_count = done.len();
    if let Some(filled) = (done_count * 20).checked_div(total) {
        let bar: String = "█".repeat(filled) + &"░".repeat(20 - filled);
        lines.push(format!("[{bar}] {done_count}/{total} done"));
    }

    if in_progress.is_empty() {
        lines.push("▸ In progress: (none)".to_string());
    } else {
        lines.push(format!("▸ In progress: {}", in_progress.len()));
        for t in &in_progress {
            let who = t.assignee.as_deref().unwrap_or("?");
            let stale = stale_marker(&t.updated_at, &now, 4);
            lines.push(format!("  🟠 {} — {}{} [{}]", t.title, who, stale, t.id));
        }
    }

    if !blocked.is_empty() {
        lines.push(format!("▸ Blocked: {}", blocked.len()));
        for t in &blocked {
            lines.push(format!("  🔴 {} [{}]", t.title, t.id));
        }
    }

    if !open.is_empty() {
        lines.push(format!("▸ Open (backlog): {}", open.len()));
        for t in open.iter().take(5) {
            let stale = stale_marker(&t.updated_at, &now, 24);
            lines.push(format!("  ⚪ {}{} [{}]", t.title, stale, t.id));
        }
        if open.len() > 5 {
            lines.push(format!("  ... +{} more", open.len() - 5));
        }
    }

    if !done.is_empty() {
        lines.push(format!("▸ Done: {}", done.len()));
        for t in done.iter().take(5) {
            lines.push(format!("  ✅ {}", t.title));
        }
    }

    let active_decisions: Vec<_> = decisions.iter().take(3).collect();
    if !active_decisions.is_empty() {
        lines.push(format!("▸ Active decisions: {}", decisions.len()));
        for d in &active_decisions {
            lines.push(format!("  📋 {} [{}]", d.title, d.id));
        }
    }

    lines.join("\n")
}

fn stale_marker(
    updated_at: &str,
    now: &chrono::DateTime<chrono::Utc>,
    threshold_hours: i64,
) -> &'static str {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .map(|dt| {
            if now.signed_duration_since(dt) > chrono::Duration::hours(threshold_hours) {
                " ⚠️stale"
            } else {
                ""
            }
        })
        .unwrap_or("")
}

/// Check if a message text is a status keyword trigger.
pub fn is_status_keyword(text: &str) -> bool {
    let trimmed = text.trim();
    matches!(
        trimmed,
        "狀況" | "summary" | "現在" | "進度" | "status" | "進度？" | "狀況？"
    )
}

/// Check if a message is a task creation request. Returns the task title if matched.
pub fn parse_task_entry(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("加 task:")
        .or_else(|| trimmed.strip_prefix("加 task："))
        .or_else(|| trimmed.strip_prefix("add task:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Auto-close tasks whose branch merged. Only closes tasks in `verified` status
/// (v1.2 §10.3 lifecycle: verified → done). Skips ambiguous matches.
pub fn auto_close_merged_tasks(home: &Path, branch: &str) {
    let tasks = crate::tasks::list_all(home);
    let candidates: Vec<_> = tasks
        .iter()
        .filter(|t| t.status == "verified")
        .filter(|t| {
            contains_as_token(&t.description, branch) || contains_as_token(&t.title, branch)
        })
        .collect();

    if candidates.is_empty() {
        return;
    }
    if candidates.len() > 1 {
        let ids: Vec<_> = candidates.iter().map(|t| t.id.as_str()).collect();
        tracing::warn!(
            branch,
            count = candidates.len(),
            ?ids,
            "ambiguous branch→task match, skipping auto-close"
        );
        return;
    }

    let task = candidates[0];
    let result = format!("auto-closed: branch '{}' merged", branch);
    // M4: emit Done event directly via task_events::append, bypassing
    // handle() which requires ACL check against "system" sender.
    let emitter = crate::task_events::InstanceName::from("system:auto_close");
    let event = crate::task_events::TaskEvent::Done {
        task_id: crate::task_events::TaskId(task.id.clone()),
        by: emitter.clone(),
        source: crate::task_events::DoneSource::OperatorManual {
            authored_at: chrono::Utc::now().to_rfc3339(),
            result: Some(result),
        },
    };
    match crate::task_events::append(home, &emitter, event) {
        Ok(_) => tracing::info!(task_id = %task.id, branch, "auto-closed task on PR merge"),
        Err(e) => tracing::warn!(error = %e, task_id = %task.id, "auto-close event append failed"),
    }
}

/// Check if `haystack` contains `needle` as a whole token (word-boundary match).
fn contains_as_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word(haystack.as_bytes()[abs - 1]);
        let after = abs + needle.len();
        let after_ok = after >= haystack.len() || !is_word(haystack.as_bytes()[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn keyword_positive_matches() {
        assert!(is_status_keyword("狀況"));
        assert!(is_status_keyword("summary"));
        assert!(is_status_keyword("現在"));
        assert!(is_status_keyword("進度"));
        assert!(is_status_keyword("status"));
        assert!(is_status_keyword("  狀況  "));
        assert!(is_status_keyword("進度？"));
    }

    #[test]
    fn keyword_negative_no_partial() {
        assert!(!is_status_keyword("進度比上週好"));
        assert!(!is_status_keyword("summary of changes"));
        assert!(!is_status_keyword("hello"));
        assert!(!is_status_keyword(""));
    }

    #[test]
    fn build_summary_does_not_panic_on_empty_home() {
        let dir = std::env::temp_dir().join(format!("agend-summary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let summary = build_summary(&dir);
        assert!(summary.contains("Status Summary"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_task_entry_positive() {
        assert_eq!(parse_task_entry("加 task: fix bug"), Some("fix bug"));
        assert_eq!(
            parse_task_entry("add task: new feature"),
            Some("new feature")
        );
        assert_eq!(parse_task_entry("加 task：中文描述"), Some("中文描述"));
    }

    #[test]
    fn parse_task_entry_negative() {
        assert!(parse_task_entry("hello").is_none());
        assert!(parse_task_entry("加 task:").is_none());
        assert!(parse_task_entry("加 task: ").is_none());
    }

    #[test]
    fn stale_marker_fresh_is_empty() {
        let now = chrono::Utc::now();
        let recent = (now - chrono::Duration::hours(1)).to_rfc3339();
        assert_eq!(stale_marker(&recent, &now, 4), "");
    }

    #[test]
    fn stale_marker_old_shows_warning() {
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::hours(5)).to_rfc3339();
        assert!(stale_marker(&old, &now, 4).contains("stale"));
    }

    #[test]
    fn contains_as_token_word_boundary() {
        assert!(contains_as_token(
            "branch=sprint18-task-board-phase2",
            "sprint18-task-board-phase2"
        ));
        assert!(contains_as_token(
            "sprint18-task-board-phase2 merged",
            "sprint18-task-board-phase2"
        ));
        assert!(!contains_as_token(
            "sprint18-task-board-phase2-extra",
            "sprint18-task-board-phase2"
        ));
        assert!(!contains_as_token("", "test"));
        assert!(!contains_as_token("test", ""));
    }

    #[test]
    fn auto_close_skips_unverified_task() {
        // F1 invariant: only verified → done, never skip review
        let dir = std::env::temp_dir().join(format!("agend-autoclose-f1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        // Create a task in "claimed" status with branch name in description
        crate::tasks::handle(
            &dir,
            "test",
            &serde_json::json!({
                "action": "create",
                "title": "test task",
                "description": "branch=sprint18-test-branch",
            }),
        );
        auto_close_merged_tasks(&dir, "sprint18-test-branch");
        let tasks = crate::tasks::list_all(&dir);
        let task = tasks.iter().find(|t| t.title == "test task").unwrap();
        assert_ne!(
            task.status, "done",
            "unverified task must NOT be auto-closed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
