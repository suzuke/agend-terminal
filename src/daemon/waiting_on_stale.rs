//! Issue #651 — `waiting_on` stale detection.
//!
//! Daemon periodically scans all agents with an active `waiting_on`
//! condition. When `waiting_on_since` exceeds 15 minutes, emits an
//! inbox alert to the agent itself AND its team orchestrator (if any).
//!
//! Uses the standard tracker pattern (tick_count + TICKS_PER_SCAN
//! throttle) consistent with `idle_watchdog`, `anti_stall`, etc.

use std::collections::HashMap;
use std::path::Path;

/// Stale threshold: 15 minutes in seconds.
const STALE_THRESHOLD_SECS: i64 = 15 * 60;

/// Re-alert suppression: 30 minutes between repeated alerts for the
/// same agent.
const REALERT_INTERVAL_SECS: i64 = 30 * 60;

/// Scan throttle: 30 ticks × 10s = 5 min cadence (matches other
/// watchdogs).
const TICKS_PER_SCAN: u64 = 30;

#[derive(Debug, Default)]
pub(crate) struct WaitingOnStaleTracker {
    tick_count: u64,
    /// agent → last alert timestamp (dedup guard).
    last_alerted_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl WaitingOnStaleTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
        scan_and_emit(home, &mut self.last_alerted_at);
        true
    }
}

/// Scan all metadata files for stale `waiting_on` conditions and emit
/// alerts. Exposed `pub(crate)` for unit tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) {
    let now = chrono::Utc::now();
    let meta_dir = home.join("metadata");
    let Ok(entries) = std::fs::read_dir(&meta_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(agent) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(condition) = meta.get("waiting_on").and_then(|v| v.as_str()) else {
            continue;
        };
        if condition.is_empty() {
            continue;
        }
        let Some(since_str) = meta.get("waiting_on_since").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(since) = chrono::DateTime::parse_from_rfc3339(since_str) else {
            continue;
        };
        let elapsed_secs = now.signed_duration_since(since).num_seconds();
        if elapsed_secs < STALE_THRESHOLD_SECS {
            continue;
        }
        // Dedup: skip if already alerted within REALERT_INTERVAL_SECS
        if let Some(prev) = last_alerted.get(agent) {
            if now.signed_duration_since(*prev).num_seconds() < REALERT_INTERVAL_SECS {
                continue;
            }
        }
        let elapsed_min = elapsed_secs / 60;
        emit_stale_alert(home, agent, condition, elapsed_min);
        last_alerted.insert(agent.to_string(), now);
    }
}

fn emit_stale_alert(home: &Path, agent: &str, condition: &str, elapsed_min: i64) {
    let text = format!("[waiting_on_stale] {agent}: waiting on \"{condition}\" for {elapsed_min}m");
    // Alert the agent itself
    emit_to(home, agent, "waiting_on_stale", &text, Some(agent));
    // Alert team orchestrator (if any)
    if let Some(team) = crate::teams::find_team_for(home, agent) {
        if let Some(ref orch) = team.orchestrator {
            if orch != agent {
                emit_to(home, orch, "waiting_on_stale", &text, Some(agent));
            }
        }
    }
}

fn emit_to(home: &Path, recipient: &str, kind: &str, text: &str, correlation_agent: Option<&str>) {
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: format!("system:{kind}"),
        text: text.to_string(),
        kind: Some(kind.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: Some("inbox_fallback".to_string()),
        task_id: None,
        force_meta: None,
        correlation_id: correlation_agent.map(String::from),
        reviewed_head: None,
        attachments: Vec::new(),
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        sequencing: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
        pr_number: None,
    };
    if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, recipient, msg) {
        tracing::warn!(error = %e, recipient, kind, "waiting_on_stale: enqueue failed");
    } else {
        tracing::info!(
            recipient,
            agent = correlation_agent.unwrap_or(""),
            "waiting_on_stale: emitted alert"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("agend-test-waiting-stale-{tag}-{id}"))
    }

    fn write_metadata(home: &Path, agent: &str, waiting_on: &str, since: &str) {
        let dir = home.join("metadata");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "waiting_on": waiting_on,
            "waiting_on_since": since,
        });
        std::fs::write(
            dir.join(format!("{agent}.json")),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn detects_stale_waiting_on() {
        let home = tmp_home("detect");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-1", "review from reviewer", &since);
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);

        assert!(last_alerted.contains_key("dev-1"));
        let inbox_file = home.join("inbox").join("dev-1.jsonl");
        assert!(inbox_file.exists(), "inbox file should exist");
        let content = std::fs::read_to_string(&inbox_file).unwrap();
        assert!(content.contains("waiting_on_stale"));
        assert!(content.contains("review from reviewer"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn skips_fresh_waiting_on() {
        let home = tmp_home("fresh");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        write_metadata(&home, "dev-2", "CI result", &since);

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);

        assert!(!last_alerted.contains_key("dev-2"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn dedup_suppresses_repeated_alert() {
        let home = tmp_home("dedup");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-3", "task from lead", &since);
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        assert!(last_alerted.contains_key("dev-3"));

        let count_lines = || {
            std::fs::read_to_string(home.join("inbox").join("dev-3.jsonl"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let first_count = count_lines();

        scan_and_emit(&home, &mut last_alerted);
        assert_eq!(
            count_lines(),
            first_count,
            "dedup should suppress second alert"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn tracker_throttles_scans() {
        let home = tmp_home("throttle");
        let mut tracker = WaitingOnStaleTracker::default();
        for _ in 0..29 {
            assert!(!tracker.maybe_scan(&home));
        }
        assert!(tracker.maybe_scan(&home));
        assert!(!tracker.maybe_scan(&home));
        let _ = std::fs::remove_dir_all(&home);
    }
}
