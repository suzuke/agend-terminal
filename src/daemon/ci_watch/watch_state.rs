use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscriber {
    pub instance: String,
    #[serde(default)]
    pub subscribed_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    #[serde(default)]
    pub repo: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_provider_url: Option<String>,

    // Subscriber list (Sprint 54 P0-1 canonical form)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribers: Option<Vec<Subscriber>>,
    // Legacy single-instance field (deprecated, kept for one release cycle)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,

    // Poll state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_polled_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_interval_secs: Option<u64>,

    // Notification dedup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notified_head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notified_conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stale_emitted_sha: Option<String>,

    // #1326: job-level early-fail dedup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early_fail_notified_sha: Option<String>,

    // TTL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_terminal_seen_at: Option<String>,

    // Two-consecutive-terminal guard (#1267)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_since: Option<String>,

    // Mergeable state (#813 periodic recheck)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mergeable_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mergeable_check_at: Option<String>,

    // Rate-limit backoff
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_limit: Option<u64>,

    // Stall tracking
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_skips: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_notified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_since_ms: Option<i64>,

    // Routing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after_ci: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_class: Option<String>,
    /// #1151: when set, only these workflow names count toward the
    /// "CI passed" aggregate. Non-required checks (e.g. flaky Windows)
    /// are ignored. When absent, all checks must pass (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_checks: Option<Vec<String>>,
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_interval() -> u64 {
    60
}

impl WatchState {
    pub fn subscriber_names(&self) -> Vec<String> {
        if let Some(subs) = &self.subscribers {
            let mut out: Vec<String> = subs
                .iter()
                .filter(|s| !s.instance.is_empty())
                .map(|s| s.instance.clone())
                .collect();
            out.sort();
            out.dedup();
            if !out.is_empty() {
                return out;
            }
        }
        if let Some(legacy) = &self.instance {
            if !legacy.is_empty() {
                return vec![legacy.clone()];
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn minimal_json_deserializes_with_defaults() {
        let json = r#"{"repo": "owner/repo"}"#;
        let ws: WatchState = serde_json::from_str(json).unwrap();
        assert_eq!(ws.repo, "owner/repo");
        assert_eq!(ws.branch, "main");
        assert_eq!(ws.interval_secs, 60);
        assert!(ws.last_run_id.is_none());
        assert!(ws.subscribers.is_none());
    }

    #[test]
    fn full_json_round_trip() {
        let json = serde_json::json!({
            "repo": "owner/repo",
            "branch": "feat/x",
            "interval_secs": 120,
            "ci_provider": "github",
            "ci_provider_url": "https://api.github.com",
            "subscribers": [
                {"instance": "dev-1", "subscribed_at": "2026-01-01T00:00:00Z"}
            ],
            "instance": "dev-1",
            "last_run_id": 42,
            "head_sha": "abc1234",
            "last_polled_at": 1700000000000_i64,
            "effective_interval_secs": 120,
            "last_notified_head_sha": "abc1234",
            "last_notified_conclusion": "success",
            "last_stale_emitted_sha": null,
            "expires_at": "2026-01-04T00:00:00Z",
            "last_terminal_seen_at": "2026-01-01T01:00:00Z",
            "rate_limit_until": 1700001000_u64,
            "rate_limit_remaining": 4500,
            "rate_limit_limit": 5000,
            "consecutive_skips": 0,
            "stalled_notified": false,
            "stalled_since_ms": null,
            "next_after_ci": "reviewer",
            "task_id": "t-123",
            "review_class": "single",
        });
        let ws: WatchState = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(ws.repo, "owner/repo");
        assert_eq!(ws.branch, "feat/x");
        assert_eq!(ws.interval_secs, 120);
        assert_eq!(ws.last_run_id, Some(42));
        assert_eq!(ws.head_sha.as_deref(), Some("abc1234"));
        assert_eq!(ws.next_after_ci.as_deref(), Some("reviewer"));
        assert_eq!(ws.consecutive_skips, Some(0));
        assert_eq!(ws.stalled_notified, Some(false));
        assert!(ws.stalled_since_ms.is_none());

        let re_serialized = serde_json::to_value(&ws).unwrap();
        assert_eq!(re_serialized["repo"], "owner/repo");
        assert_eq!(re_serialized["branch"], "feat/x");
        assert_eq!(re_serialized["interval_secs"], 120);
        assert_eq!(re_serialized["last_run_id"], 42);
        assert_eq!(re_serialized["next_after_ci"], "reviewer");
    }

    #[test]
    fn legacy_instance_only_json() {
        let json = r#"{"repo": "owner/repo", "branch": "main", "instance": "dev-1"}"#;
        let ws: WatchState = serde_json::from_str(json).unwrap();
        assert!(ws.subscribers.is_none());
        assert_eq!(ws.instance.as_deref(), Some("dev-1"));
        let names = ws.subscriber_names();
        assert_eq!(names, vec!["dev-1"]);
    }

    #[test]
    fn subscriber_names_dedupes() {
        let json = serde_json::json!({
            "repo": "o/r",
            "subscribers": [
                {"instance": "a"},
                {"instance": "a"},
                {"instance": "b"},
            ]
        });
        let ws: WatchState = serde_json::from_value(json).unwrap();
        let names = ws.subscriber_names();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn null_fields_deserialize_as_none() {
        let json = serde_json::json!({
            "repo": "o/r",
            "last_run_id": null,
            "head_sha": null,
            "last_polled_at": null,
            "stalled_since_ms": null,
        });
        let ws: WatchState = serde_json::from_value(json).unwrap();
        assert!(ws.last_run_id.is_none());
        assert!(ws.head_sha.is_none());
        assert!(ws.last_polled_at.is_none());
        assert!(ws.stalled_since_ms.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"repo": "o/r", "unknown_future_field": 42}"#;
        let ws: WatchState = serde_json::from_str(json).unwrap();
        assert_eq!(ws.repo, "o/r");
    }

    #[test]
    fn handler_created_watch_json_round_trips() {
        let json = serde_json::json!({
            "repo": "suzuke/agend-terminal",
            "branch": "fix/1084-watchdog-snooze",
            "interval_secs": 60,
            "ci_provider": null,
            "ci_provider_url": null,
            "last_run_id": null,
            "head_sha": null,
            "last_polled_at": null,
            "last_notified_head_sha": null,
            "expires_at": "2026-05-26T14:47:25.263Z",
            "last_terminal_seen_at": null,
            "subscribers": [{"instance": "fixup-dev", "subscribed_at": "2026-05-23T14:47:25Z"}],
            "instance": "fixup-dev",
            "next_after_ci": "fixup-reviewer",
            "task_id": "t-20260523144725263493-10",
        });
        let ws: WatchState = serde_json::from_value(json).unwrap();
        assert_eq!(ws.repo, "suzuke/agend-terminal");
        assert_eq!(ws.branch, "fix/1084-watchdog-snooze");
        assert_eq!(ws.next_after_ci.as_deref(), Some("fixup-reviewer"));
        assert_eq!(ws.task_id.as_deref(), Some("t-20260523144725263493-10"));
        let names = ws.subscriber_names();
        assert_eq!(names, vec!["fixup-dev"]);
    }
}
