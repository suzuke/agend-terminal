//! #3159: `ci unwatch` — the generic branch unsubscribe plus the addressed
//! exact-head disarm, split out of `watch.rs` so
//! that handler stays under the MCP-handler LOC ceiling. The exact-head key
//! space is deliberately isolated here: it never falls back to the generic
//! `repo:branch` watch, and its authority ladder is no weaker than the arm's.

use serde_json::{json, Value};
use std::path::Path;

/// Count the still-ARMED exact-head watches for `repo`@`branch` (tombstoned
/// ones are excluded — they are already disarmed). #3159: the generic unwatch
/// response used to claim `watching:false` while post-merge exact-head watches
/// stayed armed, so both response arms disclose this number.
pub(super) fn armed_exact_head_count(home: &Path, repo: &str, branch: &str) -> u64 {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|w| {
            w["repo"].as_str() == Some(repo)
                && w["branch"].as_str().unwrap_or("main") == branch
                && w["target_head_sha"].as_str().is_some_and(|s| !s.is_empty())
                && w["auto_arm_optout"].as_bool() != Some(true)
        })
        .count() as u64
}

/// #3159: disarm ONE exact-head post-merge watch addressed by its immutable
/// identity. Split from [`handle_unwatch_ci`] so the exact-head key space can
/// never fall back to the generic `repo:branch` watch: a malformed, unknown, or
/// identity-mismatched SHA is an ERROR, never a silent generic unwatch (a typo
/// must not drop the whole branch's watch).
///
/// Authority is deliberately NO WEAKER than the arm that created the watch
/// (`handle_watch_ci`'s protected-ref gate): operator (empty caller) passes;
/// a `notification_only` watch requires the merge-receipt task assignee; a
/// privileged watch requires orchestrator authority over EVERY persisted
/// `next_after_ci` continuation target. Anything else is rejected — a weaker
/// gate would re-open the #2622/#1575 cross-agent class this handler was
/// hardened against.
pub(super) fn unwatch_exact_head(
    home: &Path,
    repo: &str,
    branch: &str,
    raw_sha: &Value,
    caller: &str,
) -> Value {
    // Presence already selected this path; only a FULL hex string may proceed.
    let sha = raw_sha.as_str().unwrap_or_default();
    if !crate::daemon::ci_watch::is_full_commit_sha(sha) {
        return json!({
            "error": format!("exact-head unwatch requires a FULL immutable commit SHA (40- or 64-hex); got {raw_sha}"),
            "code": "exact_head_unwatch_invalid_sha",
        });
    }
    let sha = crate::daemon::ci_watch::normalize_head_sha(sha);
    let filename = crate::daemon::ci_watch::watch_filename_exact_head(repo, branch, &sha);
    let path = crate::daemon::ci_watch::ci_watches_dir(home).join(&filename);
    let _watch_lock = crate::store::acquire_file_lock(&path.with_extension("lock"));
    let Some(mut watch) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return json!({
            "error": format!("no exact-head watch for {repo}@{branch} at {sha}"),
            "code": "exact_head_unwatch_not_found",
        });
    };
    // Identity re-proof under the lock: the file's own target must equal the
    // addressed SHA (a hash collision or a hand-edited file fails closed).
    if watch["target_head_sha"].as_str() != Some(sha.as_str()) {
        return json!({
            "error": "addressed exact-head watch does not carry the requested target_head_sha",
            "code": "exact_head_unwatch_identity_mismatch",
        });
    }

    // Authority ladder, strongest first. FULL DISARM is reserved for the
    // operator and for orchestrator authority over this watch; a merge-receipt
    // task assignee is NOT a full-disarm principal and may only drop its own
    // subscription (it must never erase co-subscribers or continuations).
    let notification_only = watch["notification_only"].as_bool().unwrap_or(false);
    let receipt = if notification_only {
        crate::merge_receipt::find(home, repo, &sha, watch["task_id"].as_str().unwrap_or(""))
    } else {
        None
    };
    let orchestrator_authority = !caller.is_empty()
        && if notification_only {
            // A notification_only watch carries no `next_after_ci` (forbidden at
            // arm time), so orchestrator authority is proven against the receipt:
            // the recorded merge authority, or the assignee's orchestrator.
            receipt.as_ref().is_some_and(|r| {
                r.merge_authority == caller
                    || crate::teams::is_orchestrator_of(home, caller, &r.task_assignee)
            })
        } else {
            let targets = crate::daemon::ci_watch::watch_state::normalize_next_after_ci(
                watch.get("next_after_ci").unwrap_or(&Value::Null),
            );
            !targets.is_empty()
                && targets
                    .iter()
                    .all(|m| crate::teams::is_orchestrator_of(home, caller, m))
        };

    if !caller.is_empty() && !orchestrator_authority {
        // Not a full-disarm principal. The ONLY remaining legitimate caller is
        // the notification_only watch's own merge-receipt assignee, dropping its
        // own subscription. The durable proof is the receipt (repo + SHA +
        // task_id + assignee), never a current binding: by disarm time a
        // legitimate assignee is usually bound to a later, unrelated task.
        let Some(receipt) = receipt.filter(|r| r.task_assignee == caller) else {
            return json!({
                "error": format!(
                    "'{caller}' may not disarm this exact-head watch — full disarm requires the operator or orchestrator authority; a merge-receipt assignee may only drop its own subscription"
                ),
                "code": "exact_head_unwatch_unauthorized",
            });
        };
        let _ = &receipt;
        let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
        let was_subscribed = subscribers.iter().any(|s| s == caller);
        subscribers.retain(|s| s != caller);
        let subscribers_json: Vec<Value> = subscribers
            .iter()
            .map(|name| {
                let prior = watch
                    .get("subscribers")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| {
                        arr.iter().find(|s| {
                            s.get("instance").and_then(|i| i.as_str()) == Some(name.as_str())
                        })
                    })
                    .and_then(|s| s.get("subscribed_at").and_then(|v| v.as_str()))
                    .map(String::from)
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                json!({"instance": name, "subscribed_at": prior})
            })
            .collect();
        watch["subscribers"] = json!(subscribers_json);
        watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());
        // Terminal rule, mirroring the generic path: dropping the LAST
        // subscriber leaves a watch the poller classifies Invalid
        // (subscriberless) — an armed-looking zombie that `armed_exact_head_count`
        // would keep counting. Tombstone it instead, so state and response agree.
        // No collateral principal is affected: by construction nobody else is
        // subscribed here.
        let tombstoned = subscribers.is_empty();
        if tombstoned {
            watch["auto_arm_optout"] = json!(true);
            watch["unwatched_at"] = json!(chrono::Utc::now().to_rfc3339());
        }
        if let Err(e) = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch)
                .unwrap_or_default()
                .as_bytes(),
        ) {
            return json!({
                "error": format!("failed to persist exact-head unsubscribe: {e}"),
                "code": "unwatch_write_failed",
            });
        }
        return json!({
            "repo": repo,
            "branch": branch,
            "scope": "exact_head",
            "head_sha": sha,
            "watching": !subscribers.is_empty(),
            "disarmed": tombstoned,
            "unsubscribed": was_subscribed,
            "subscribers": subscribers,
            "exact_head_remaining": armed_exact_head_count(home, repo, branch),
        });
    }

    // Authorized: disarm the addressed watch. Tombstone (not delete) for the
    // same #1991 reason the generic path does — a deleted file invites re-arm.
    watch["subscribers"] = json!([]);
    watch["instance"] = json!("");
    watch["auto_arm_optout"] = json!(true);
    watch["unwatched_at"] = json!(chrono::Utc::now().to_rfc3339());
    if let Err(e) = crate::store::atomic_write(
        &path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        return json!({
            "error": format!("failed to persist exact-head disarm: {e}"),
            "code": "unwatch_write_failed",
        });
    }
    json!({
        "repo": repo,
        "branch": branch,
        "scope": "exact_head",
        "head_sha": sha,
        "watching": false,
        "disarmed": true,
        "exact_head_remaining": armed_exact_head_count(home, repo, branch),
    })
}

/// `ci unwatch` — unsubscribe caller from repo@branch, or (with `head_sha`)
/// disarm one exact-head post-merge watch (#3159).
pub(crate) fn handle_unwatch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repo = match args["repository"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repository'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    // #3159: an addressed exact-head disarm is a DIFFERENT key space; it never
    // touches — nor falls back to — the generic `repo:branch` watch below.
    // PRESENCE of the key is the whole selector: an empty string, a non-string,
    // an explicit `null`, or a malformed SHA all fail closed inside
    // `unwatch_exact_head`. Downgrading ANY present-but-unusable selector to a
    // whole-branch unwatch is the exact footgun this feature exists to remove,
    // and the ci schema declares `head_sha` as a string, so a null is a caller
    // error rather than an inert transport artifact.
    if let Some(raw) = args.get("head_sha") {
        return unwatch_exact_head(home, repo, branch, raw, instance_name);
    }
    // Caller identity for selective removal is ALWAYS the MCP-validated sender.
    // (#2622-followup t-20260705161926295621-30532-2 ②, decision
    // d-20260705165815268234-1): the former `args["instance"]` override was an
    // unauthenticated cross-agent footgun — agent A could pass
    // `instance="agent-B"` to silently drop B's CI-watch subscription AND
    // resolve B's ci-handoff obligation track (the #2622 obligation-loss class,
    // B never notified). It had no production caller, no test, and was
    // undeclared in the schema, so it is REMOVED rather than gated (name-based
    // cross-agent authority is the #1575 class). A legitimate "clean a dead
    // agent's subscription" need would be a separate authenticated + audited
    // surface, not a silent arg on a general agent tool. The empty-caller
    // `subscribers.clear()` path below stays as a defensive fallback (an MCP
    // call always supplies a non-empty validated sender).
    let caller = instance_name.to_string();
    // #t-92758 P2: unwatch is also the lead's dismiss path for a stuck ci-ready —
    // clear the caller's own ci-handoff track for this repo@branch so the re-nudge
    // watchdog stops (previously unwatch removed the watch subscription but NOT the
    // decoupled ci-ready obligation, so re-nudges continued). Done unconditionally
    // (even if the watch file is already absent below) since the intent is to drop
    // the obligation. Precise (caller + exact correlation) so a co-subscriber's
    // track is left intact.
    if !caller.is_empty() {
        let correlation = format!("{repo}@{branch}");
        crate::daemon::ci_handoff_track::resolve_for_target_correlation(
            home,
            &caller,
            &correlation,
        );
    }
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = crate::daemon::ci_watch::ci_watches_dir(home).join(&filename);
    // H5: flock the per-watch read→atomic_write RMW (see handle_watch_ci).
    let _watch_lock = crate::store::acquire_file_lock(&path.with_extension("lock"));

    let mut watch = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(v) => v,
        None => {
            // No watch file at all — idempotent no-op (matches pre-r0 behavior).
            // #3159: `watching:false` is scoped to the GENERIC key; disclose any
            // still-armed exact-head watches so this is not read as repo-wide.
            return json!({
                "repo": repo,
                "branch": branch,
                "scope": "generic",
                "watching": false,
                "subscribers": Vec::<String>::new(),
                "exact_head_remaining": armed_exact_head_count(home, repo, branch),
            });
        }
    };

    let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
    if !caller.is_empty() {
        subscribers.retain(|s| s != &caller);
    } else {
        // No caller identity (unauthenticated/operator call) — clear ALL.
        subscribers.clear();
    }

    if subscribers.is_empty() {
        // #1991: keep the file as a TOMBSTONE instead of deleting it. PR-3
        // auto-arm (`pr_state::auto_arm`) re-arms any open PR whose watch file
        // is ABSENT — deleting here re-subscribed the very agent that just
        // unwatched, ~60s later (the #1991 storm: unwatch → file gone → next
        // pr_state scan auto-arms → notifications resume). Unwatch is an
        // EXPLICIT decision: the tombstone suppresses auto-arm until the PR
        // goes terminal or someone explicitly re-watches (handle_watch_ci
        // clears the flag). It is never polled (`prepare_poll_context` →
        // SkipReason::Invalid, zero API budget) and gc exempts it from the
        // TTL/inactivity reaps (P6: a TTL-reap → re-arm is the same betrayal,
        // only slower); end-of-life = PR-terminal gc or the unwatched_at
        // age-cap backstop.
        watch["subscribers"] = json!([]);
        watch["instance"] = json!("");
        watch["auto_arm_optout"] = json!(true);
        watch["unwatched_at"] = json!(chrono::Utc::now().to_rfc3339());
        if let Err(e) = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch)
                .unwrap_or_default()
                .as_bytes(),
        ) {
            return json!({
                "error": format!("failed to persist unwatch tombstone: {e}"),
                "code": "unwatch_write_failed",
            });
        }
        return json!({
            "repo": repo,
            "branch": branch,
            "scope": "generic",
            "watching": false,
            "subscribers": Vec::<String>::new(),
            "tombstone": true,
            "exact_head_remaining": armed_exact_head_count(home, repo, branch),
        });
    }

    let subscribers_json: Vec<Value> = subscribers
        .iter()
        .map(|name| {
            let prior = watch
                .get("subscribers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|s| s.get("instance").and_then(|i| i.as_str()) == Some(name.as_str()))
                })
                .and_then(|s| s.get("subscribed_at").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            json!({"instance": name, "subscribed_at": prior})
        })
        .collect();
    watch["subscribers"] = json!(subscribers_json);
    watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());

    if let Err(e) = crate::store::atomic_write(
        &path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        return json!({
            "error": format!("failed to persist unwatch: {e}"),
            "code": "unwatch_write_failed",
        });
    }
    json!({
        "repo": repo,
        "branch": branch,
        "scope": "generic",
        "watching": true,
        "subscribers": subscribers,
        "exact_head_remaining": armed_exact_head_count(home, repo, branch),
    })
}
