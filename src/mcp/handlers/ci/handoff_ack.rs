use serde_json::{json, Value};
use std::path::Path;

use crate::inbox::storage::{
    handoff_row_state, settle_ci_handoff_row_exact, HandoffRowSettleOutcome,
    ProtectedHandoffRowState,
};

/// Neutral pickup settlement for a system-origin `ci-ready-for-action` row.
/// It is deliberately narrower than `unwatch` and channel `discharge`: the
/// caller may settle only its own exact episode, and the watch stays armed.
pub(crate) fn handle_ack_handoff_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    if instance_name.is_empty() {
        return json!({
            "error": "ack_handoff requires an authenticated instance caller",
            "code": "caller_identity_required"
        });
    }
    let raw_repo = match args["repository"].as_str().filter(|s| !s.is_empty()) {
        Some(repo) => repo,
        None => {
            return json!({"error": "missing required 'repository'", "code": "missing_repository"})
        }
    };
    let repository = match crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(raw_repo) {
        Some(repo) => repo,
        None => return json!({"error": "invalid repository", "code": "invalid_repo_format"}),
    };
    let branch = match args["branch"].as_str().filter(|s| !s.is_empty()) {
        Some(branch) => branch,
        None => return json!({"error": "missing required 'branch'", "code": "missing_branch"}),
    };
    let episode = match args["episode"].as_str().filter(|s| !s.is_empty()) {
        Some(episode) => episode,
        None => return json!({"error": "missing required 'episode'", "code": "missing_episode"}),
    };
    let correlation = format!("{repository}@{branch}");
    let tracks = crate::daemon::ci_handoff_track::list(home);
    let same_key: Vec<_> = tracks
        .iter()
        .filter(|(_, track)| track.target == instance_name && track.correlation == correlation)
        .collect();
    let exact: Vec<_> = same_key
        .iter()
        .filter(|(_, track)| track.ci_handoff_episode.as_deref() == Some(episode))
        .collect();

    if exact.is_empty() {
        if !same_key.is_empty() {
            return json!({"error": "episode mismatch (CAS)", "code": "episode_mismatch"});
        }
        for class in [
            crate::inbox::CiHandoffClass::Feature,
            crate::inbox::CiHandoffClass::Protected,
        ] {
            if handoff_row_state(home, instance_name, &correlation, episode, class)
                == ProtectedHandoffRowState::ExplicitlyAcked
            {
                return json!({
                    "ok": true,
                    "acked": true,
                    "already_acked": true,
                    "correlation": correlation,
                    "episode": episode,
                    "watch_preserved": true
                });
            }
        }
        // Draft/REJECTED resolution may remove the sidecar before the exact
        // recipient ACKs the feature handoff. The unique trusted feature row
        // remains the only authority in that race; settle it without touching
        // the watch or inferring a newer episode.
        let feature_row_state = handoff_row_state(
            home,
            instance_name,
            &correlation,
            episode,
            crate::inbox::CiHandoffClass::Feature,
        );
        if matches!(
            feature_row_state,
            ProtectedHandoffRowState::Pending
                | ProtectedHandoffRowState::Processed
                | ProtectedHandoffRowState::Ambiguous
        ) {
            let row_outcome = settle_ci_handoff_row_exact(
                home,
                instance_name,
                &correlation,
                episode,
                crate::inbox::CiHandoffClass::Feature,
            );
            let already_acked = match row_outcome {
                HandoffRowSettleOutcome::Settled => false,
                HandoffRowSettleOutcome::AlreadySettled => true,
                HandoffRowSettleOutcome::Missing => {
                    return json!({"error": "matching inbox row not found", "code": "track_not_found"})
                }
                HandoffRowSettleOutcome::Ambiguous => {
                    return json!({"error": "matching inbox row is ambiguous", "code": "row_ambiguous"})
                }
                HandoffRowSettleOutcome::WriteFailed => {
                    return json!({"error": "failed to settle inbox row", "code": "row_write_failed"})
                }
                HandoffRowSettleOutcome::LockFailed => {
                    return json!({"error": "failed to lock inbox row", "code": "row_lock_failed"})
                }
            };
            crate::event_log::log(
                home,
                "ci_handoff_acknowledged",
                instance_name,
                &format!(
                    "correlation={correlation} episode={episode} class=Feature track_already_resolved=true watch_preserved=true"
                ),
            );
            return json!({
                "ok": true,
                "acked": true,
                "already_acked": already_acked,
                "settled_row": true,
                "track_already_resolved": true,
                "correlation": correlation,
                "episode": episode,
                "watch_preserved": true
            });
        }
        return json!({"error": "no matching handoff track", "code": "track_not_found"});
    }
    if exact.len() != 1 {
        return json!({"error": "ambiguous handoff track", "code": "track_ambiguous"});
    }
    let track = &exact[0].1;
    let Some(class) = track.ci_handoff_class else {
        return json!({"error": "handoff class missing", "code": "legacy_identity_unsupported"});
    };

    let row_outcome =
        settle_ci_handoff_row_exact(home, instance_name, &correlation, episode, class);
    let already_acked = match row_outcome {
        HandoffRowSettleOutcome::Settled => false,
        HandoffRowSettleOutcome::AlreadySettled => true,
        HandoffRowSettleOutcome::Missing => {
            return json!({"error": "matching inbox row not found", "code": "row_not_found"})
        }
        HandoffRowSettleOutcome::Ambiguous => {
            return json!({"error": "matching inbox row is ambiguous", "code": "row_ambiguous"})
        }
        HandoffRowSettleOutcome::WriteFailed => {
            return json!({"error": "failed to settle inbox row", "code": "row_write_failed"})
        }
        HandoffRowSettleOutcome::LockFailed => {
            return json!({"error": "failed to lock inbox row", "code": "row_lock_failed"})
        }
    };

    let resolved = crate::daemon::ci_handoff_track::resolve_exact_episode(
        home,
        instance_name,
        &correlation,
        episode,
        class,
        "ack_handoff",
    );
    if resolved != 1 {
        return json!({
            "error": "inbox row settled but handoff track CAS did not resolve; reconciler will retry",
            "code": "track_settlement_incomplete",
            "settled_row": true
        });
    }
    crate::event_log::log(
        home,
        "ci_handoff_acknowledged",
        instance_name,
        &format!(
            "correlation={correlation} episode={episode} class={class:?} watch_preserved=true"
        ),
    );
    json!({
        "ok": true,
        "acked": true,
        "already_acked": already_acked,
        "settled_row": true,
        "resolved_track": true,
        "correlation": correlation,
        "episode": episode,
        "watch_preserved": true
    })
}

pub(crate) fn handle_defer_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repository = match args["repository"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => {
            return json!({"error": "missing required 'repository'", "code": "missing_repository"})
        }
    };
    let branch = match args["branch"].as_str().filter(|s| !s.is_empty()) {
        Some(b) => b,
        None => return json!({"error": "missing required 'branch'", "code": "missing_branch"}),
    };
    let correlation = format!("{repository}@{branch}");
    let episode = match args["episode"].as_str().filter(|s| !s.is_empty()) {
        Some(e) => e,
        None => return json!({"error": "missing required 'episode'", "code": "missing_episode"}),
    };
    let wake_task_id = match args["wake_task_id"].as_str().filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return json!({"error": "missing required 'wake_task_id'", "code": "missing_wake_task_id"})
        }
    };
    let reason = match args["reason"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => {
            return json!({"error": "missing required non-empty 'reason'", "code": "missing_reason"})
        }
    };
    let defer_secs = match args["defer_secs"].as_i64() {
        Some(s) if (60..=3600).contains(&s) => s,
        Some(s) => {
            return json!({
                "error": format!("defer_secs {s} outside 60..3600"),
                "code": "invalid_defer_secs"
            })
        }
        None => {
            return json!({
                "error": "missing required 'defer_secs'",
                "code": "missing_defer_secs"
            })
        }
    };
    match crate::tasks::load_routed(home, wake_task_id) {
        Ok(rt) => {
            if matches!(
                rt.record().status,
                crate::task_events::TaskStatus::Done | crate::task_events::TaskStatus::Cancelled
            ) {
                return json!({
                    "error": format!("wake_task_id '{wake_task_id}' is already terminal"),
                    "code": "wake_task_terminal"
                });
            }
        }
        Err(_) => {
            return json!({
                "error": format!("wake_task_id '{wake_task_id}' not found"),
                "code": "wake_task_not_found"
            });
        }
    }

    let tracks = crate::daemon::ci_handoff_track::list(home);
    let track = tracks.iter().find(|(_, t)| {
        t.correlation == correlation.as_str()
            && t.ci_handoff_episode.as_deref() == Some(episode)
            && (instance_name.is_empty() || t.target == instance_name)
    });
    let Some((_, track)) = track else {
        return json!({"error": "no matching track found", "code": "track_not_found"});
    };
    let target = &track.target;

    use crate::daemon::ci_handoff_track::{DeferOutcome, DeferRequest};
    let req = DeferRequest {
        target,
        correlation: &correlation,
        episode,
        deferred_by: if instance_name.is_empty() {
            "operator"
        } else {
            instance_name
        },
        wake_task_id,
        reason,
        defer_secs,
    };
    match crate::daemon::ci_handoff_track::defer_track(home, &req) {
        DeferOutcome::Deferred => json!({"ok": true, "deferred": true}),
        DeferOutcome::AlreadyDeferred => {
            json!({"ok": true, "deferred": true, "already_deferred": true})
        }
        DeferOutcome::EpisodeMismatch => {
            json!({"error": "episode mismatch (CAS)", "code": "episode_mismatch"})
        }
        DeferOutcome::TrackNotFound => {
            json!({"error": "track not found under lock", "code": "track_not_found"})
        }
        DeferOutcome::LockFailed => {
            json!({"error": "lock acquisition failed", "code": "lock_failed"})
        }
    }
}
