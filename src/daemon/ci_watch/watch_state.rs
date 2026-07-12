use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscriber {
    pub instance: String,
    #[serde(default)]
    pub subscribed_at: Option<String>,
}

/// AUDIT2-009: per-workflow notify cursor. CI runs from N workflows form N parallel
/// `run_id` streams; the legacy single `last_run_id` high-water threshold silently
/// dropped a lower-id workflow's rerun-to-green (`run.id < threshold`) BEFORE the
/// attempt/conclusion check, breaking the `next_after_ci` reviewer handoff. Keyed by
/// workflow name (unnamed runs use a synthetic `#run:<id>` key), this records the
/// last-notified `(run_id, run_attempt, conclusion)` PER WORKFLOW, so a fresh
/// terminal event in any workflow is selected regardless of its id relative to
/// siblings. `run_id` is part of the cursor because a provider without an attempt
/// concept reports a rerun as a NEW id with `run_attempt == 1` — keying on
/// attempt+conclusion alone would swallow it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNotifyCursor {
    pub run_id: u64,
    pub run_attempt: u64,
    pub conclusion: String,
}

fn workflow_cursor_map_is_empty(
    m: &std::collections::BTreeMap<String, WorkflowNotifyCursor>,
) -> bool {
    m.is_empty()
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
    /// #1859 Fix B: `run_attempt` of the last-notified run. A `gh run rerun`
    /// keeps the same id/sha/conclusion and only bumps the attempt, so the dedup
    /// gates notify again when the current attempt EXCEEDS this. `None` on a
    /// legacy watch (pre-Fix-B) → the first post-upgrade notify seeds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notified_run_attempt: Option<u64>,
    /// #1991: the dedup-anchor run's OWN conclusion at last notify (the run
    /// whose id becomes `last_run_id`). `last_notified_conclusion` above is the
    /// per-sha AGGREGATE — comparing a single run's conclusion against it (the
    /// pre-#1991 gate-1 check) oscillates whenever the two legitimately differ
    /// (e.g. max-id run succeeded while a sibling workflow's verdict carried),
    /// re-selecting and re-notifying the same terminal state every poll.
    /// `None` on a legacy watch → gate 1 falls back to the aggregate field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notified_run_conclusion: Option<String>,
    /// AUDIT2-009: per-workflow notify cursors (keyed by workflow name; unnamed →
    /// `#run:<id>`). The authoritative notify gate — supersedes the single
    /// `last_run_id` threshold (kept above with reduced authority for migration /
    /// observability). Reset on head move. Absent on a legacy watch → seeded on
    /// the first post-upgrade poll from the current head's latest-per-workflow runs.
    #[serde(default, skip_serializing_if = "workflow_cursor_map_is_empty")]
    pub last_notified_by_workflow: std::collections::BTreeMap<String, WorkflowNotifyCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stale_emitted_sha: Option<String>,

    // #1326: job-level early-fail dedup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early_fail_notified_sha: Option<String>,

    // CI-fail-notify: fingerprint of the last-notified failing-check SET, so a
    // re-notify fires only when the set of failing checks CHANGES (not on every
    // poll of the same failure, nor on a same-set rerun). Finer than the
    // conclusion-level `last_notified_conclusion` guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_set_fingerprint: Option<String>,

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
    #[serde(
        default,
        deserialize_with = "deserialize_next_after_ci",
        serialize_with = "serialize_next_after_ci",
        skip_serializing_if = "next_after_ci_is_empty"
    )]
    pub next_after_ci: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_class: Option<String>,
    /// S1 exact-head protected-main watch: an IMMUTABLE full commit SHA this
    /// watch is pinned to. When `Some`, this is a post-merge close-loop watch on
    /// a protected ref (`main`/`master`) — the poller resolves runs for THIS SHA
    /// only (via `poll_runs_for_sha`), never the branch's latest head, so a newer
    /// unrelated main push can't falsely complete it. Set only by `handle_watch_ci`
    /// under the exact-head authority gate (task_id + next_after_ci + orchestrator/
    /// operator); dispatch auto-watch never sets it. Stored lowercase. `None` on
    /// every ordinary (branch-tracking) watch — its absence is what distinguishes a
    /// legitimate exact-head protected watch (kept by `gc_stale_watches`) from a
    /// legacy generic protected watch (removed by the E4.5 migration sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_head_sha: Option<String>,
    /// #1151: when set, only these workflow names count toward the
    /// "CI passed" aggregate. Non-required checks (e.g. flaky Windows)
    /// are ignored. When absent, all checks must pass (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_checks: Option<Vec<String>>,
    /// #1991: set when `ci unwatch` empties the subscriber list. The file is
    /// kept as a TOMBSTONE instead of being deleted — PR-3 auto-arm re-arms
    /// any open PR whose watch file is ABSENT, so deleting here re-subscribed
    /// the very agent that just unwatched (the #1991 notification storm).
    /// Semantics (P6 lead adjudication): unwatch is an EXPLICIT decision that
    /// suppresses auto-arm for the PR's whole lifetime — the tombstone is
    /// never polled (`prepare_poll_context` skips it, zero API budget) and
    /// `gc_stale_watches` exempts it from the TTL/inactivity reaps (a
    /// TTL-reap → re-arm is the 60s betrayal, only slower). End-of-life:
    /// PR terminal (gc removes once `is_branch_open` is false — the same
    /// pr_state store auto-arm keys on, so removal and won't-re-arm are
    /// consistent by construction) or the `unwatched_at` age-cap backstop.
    /// An explicit `ci watch` clears the flag (a human decision overrides).
    /// Typed (not a raw JSON key) so typed read-modify-write paths
    /// (`stamp_repo_backoff`, `flush_watch_state`) can't silently drop it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_arm_optout: Option<bool>,
    /// #1991: when the tombstone was created (last subscriber unwatched).
    /// Anchor for the gc age-cap backstop — a tombstone has no subscribers,
    /// so `earliest_subscribed_at` (the normal age anchor) is None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unwatched_at: Option<String>,
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

    /// #1750 A2: the earliest `subscribed_at` across subscribers, as a stable
    /// watch-age anchor. Unlike `expires_at` / `last_polled_at`, `subscribed_at`
    /// is set once at subscription time and never refreshed by polling, so it is
    /// the only field the per-poll `refresh_expires_at` cannot perpetually push
    /// forward — exactly what an absolute-age GC backstop needs. Returns `None`
    /// when no subscriber carries a parseable timestamp (legacy/empty watch); the
    /// caller then falls back to the refreshed-TTL paths.
    pub fn earliest_subscribed_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.subscribers
            .as_ref()?
            .iter()
            .filter_map(|s| s.subscribed_at.as_deref())
            .filter_map(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .min()
    }

    pub fn next_after_ci_targets(&self) -> Vec<String> {
        self.next_after_ci.clone().unwrap_or_default()
    }
}

pub(crate) fn normalize_next_after_ci(value: &Value) -> Vec<String> {
    let mut out = match value {
        Value::String(s) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    };
    out.sort();
    out.dedup();
    out
}

pub(crate) fn next_after_ci_json(targets: &[String]) -> Option<Value> {
    match targets {
        [] => None,
        [one] => Some(Value::String(one.clone())),
        many => Some(Value::Array(
            many.iter().map(|s| Value::String(s.clone())).collect(),
        )),
    }
}

fn next_after_ci_is_empty(targets: &Option<Vec<String>>) -> bool {
    targets.as_ref().is_none_or(Vec::is_empty)
}

fn deserialize_next_after_ci<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    let targets = value
        .as_ref()
        .map(normalize_next_after_ci)
        .unwrap_or_default();
    Ok((!targets.is_empty()).then_some(targets))
}

fn serialize_next_after_ci<S>(
    targets: &Option<Vec<String>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match targets.as_deref().and_then(next_after_ci_json) {
        Some(value) => value.serialize(serializer),
        None => serializer.serialize_none(),
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
        assert_eq!(ws.next_after_ci_targets(), vec!["reviewer"]);
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
    fn next_after_ci_array_round_trips() {
        let json = serde_json::json!({
            "repo": "owner/repo",
            "next_after_ci": ["reviewer-b", "reviewer-a", "reviewer-a", ""],
        });
        let ws: WatchState = serde_json::from_value(json).unwrap();
        assert_eq!(ws.next_after_ci_targets(), vec!["reviewer-a", "reviewer-b"]);
        let re_serialized = serde_json::to_value(&ws).unwrap();
        assert_eq!(
            re_serialized["next_after_ci"],
            serde_json::json!(["reviewer-a", "reviewer-b"])
        );
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
        assert_eq!(ws.next_after_ci_targets(), vec!["fixup-reviewer"]);
        assert_eq!(ws.task_id.as_deref(), Some("t-20260523144725263493-10"));
        let names = ws.subscriber_names();
        assert_eq!(names, vec!["fixup-dev"]);
    }
}
