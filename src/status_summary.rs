//! Visibility status summary — shared builder for telegram keyword + TUI panel.

use std::path::Path;

/// Build a human-readable status summary from task board + decisions.
pub fn build_summary(home: &Path) -> String {
    let tasks = crate::tasks::list_all(home);
    let decisions = crate::decisions::list_all(home);

    let claimed: Vec<_> = tasks.iter().filter(|t| t.status == "claimed").collect();
    let open: Vec<_> = tasks.iter().filter(|t| t.status == "open").collect();
    let blocked: Vec<_> = tasks.iter().filter(|t| t.status == "blocked").collect();
    let done_recent: Vec<_> = tasks
        .iter()
        .filter(|t| t.status == "done")
        .take(5)
        .collect();

    let mut lines = Vec::new();
    lines.push("═══ Status Summary ═══".to_string());

    // In-progress
    if claimed.is_empty() {
        lines.push("▸ In progress: (none)".to_string());
    } else {
        lines.push(format!("▸ In progress: {}", claimed.len()));
        for t in &claimed {
            let who = t.assignee.as_deref().unwrap_or("?");
            lines.push(format!("  🟠 {} — {} [{}]", t.title, who, t.id));
        }
    }

    // Blocked
    if !blocked.is_empty() {
        lines.push(format!("▸ Blocked: {}", blocked.len()));
        for t in &blocked {
            lines.push(format!("  🔴 {} [{}]", t.title, t.id));
        }
    }

    // Open (backlog)
    if !open.is_empty() {
        lines.push(format!("▸ Open (backlog): {}", open.len()));
        for t in open.iter().take(5) {
            lines.push(format!("  ⚪ {} [{}]", t.title, t.id));
        }
        if open.len() > 5 {
            lines.push(format!("  ... +{} more", open.len() - 5));
        }
    }

    // Recent done
    if !done_recent.is_empty() {
        lines.push(format!("▸ Recently done: {}", done_recent.len()));
        for t in &done_recent {
            lines.push(format!("  ✅ {}", t.title));
        }
    }

    // Active decisions
    let active_decisions: Vec<_> = decisions.iter().take(3).collect();
    if !active_decisions.is_empty() {
        lines.push(format!("▸ Active decisions: {}", decisions.len()));
        for d in &active_decisions {
            lines.push(format!("  📋 {} [{}]", d.title, d.id));
        }
    }

    lines.join("\n")
}

/// Check if a message text is a status keyword trigger.
/// Returns true for exact matches like "狀況", "summary", "現在", "進度", "status".
/// Does NOT match partial strings like "進度比上週好".
pub fn is_status_keyword(text: &str) -> bool {
    let trimmed = text.trim();
    matches!(
        trimmed,
        "狀況" | "summary" | "現在" | "進度" | "status" | "進度？" | "狀況？"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_positive_matches() {
        assert!(is_status_keyword("狀況"));
        assert!(is_status_keyword("summary"));
        assert!(is_status_keyword("現在"));
        assert!(is_status_keyword("進度"));
        assert!(is_status_keyword("status"));
        assert!(is_status_keyword("  狀況  ")); // trimmed
        assert!(is_status_keyword("進度？"));
    }

    #[test]
    fn keyword_negative_no_partial() {
        assert!(!is_status_keyword("進度比上週好"));
        assert!(!is_status_keyword("summary of changes"));
        assert!(!is_status_keyword("what is the status of PR"));
        assert!(!is_status_keyword("hello"));
        assert!(!is_status_keyword(""));
    }

    #[test]
    fn build_summary_does_not_panic_on_empty_home() {
        let dir = std::env::temp_dir().join(format!("agend-summary-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let summary = build_summary(&dir);
        assert!(summary.contains("Status Summary"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
