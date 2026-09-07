use serde_json::{json, Value};
use std::path::Path;

/// `ci watch` — subscribe to CI notifications for repo@branch.
pub(crate) fn handle_watch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repo_owned = match super::resolve_repo_or_error(home, instance_name, args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let repo: &str = &repo_owned;
    let branch = args["branch"].as_str().unwrap_or("main");
    let interval = args["interval_secs"].as_u64().unwrap_or(60);

    // S1 exact-head protected-ref gate (d-20260712033954660984-4): protected
    // refs are E4.5-rejected except exact-head post-merge watches (full SHA +
    // task_id + next_after_ci/notification_only). #2812 adds notification_only.
    let exact_head_sha: Option<String> = if crate::agent_ops::is_protected_ref(branch) {
        let head_sha = match args["head_sha"].as_str().filter(|s| !s.is_empty()) {
            // Generic protected watch (no pinned SHA) → unchanged E4.5 rejection.
            None => match crate::agent_ops::ensure_not_protected_json(branch) {
                Err(e) => return e,
                Ok(()) => unreachable!("is_protected_ref ⇒ ensure_not_protected_json errs"),
            },
            Some(s) => s,
        };
        if !crate::daemon::ci_watch::is_full_commit_sha(head_sha) {
            return json!({
                "error": format!("exact-head protected watch requires a FULL immutable commit SHA (40- or 64-hex); got {head_sha:?}"),
                "code": "protected_watch_invalid_sha",
            });
        }
        let has_task_id = args["task_id"].as_str().is_some_and(|s| !s.is_empty());
        let notification_only = args["notification_only"].as_bool().unwrap_or(false);
        let next_targets = crate::daemon::ci_watch::watch_state::normalize_next_after_ci(
            args.get("next_after_ci").unwrap_or(&Value::Null),
        );

        if notification_only {
            if !has_task_id {
                return json!({
                    "error": "notification_only watch requires `task_id`",
                    "code": "notification_only_missing_task_id",
                });
            }
            if !next_targets.is_empty() {
                return json!({
                    "error": "notification_only watch forbids `next_after_ci` — no privileged continuation allowed",
                    "code": "notification_only_next_after_ci_forbidden",
                });
            }
            let task_id = args["task_id"].as_str().unwrap_or("");
            if instance_name.is_empty() {
                return json!({
                    "error": "notification_only watch requires an identified caller (not operator/empty)",
                    "code": "notification_only_empty_caller",
                });
            }
            let Some(receipt) = crate::merge_receipt::find(home, repo, head_sha, task_id) else {
                return json!({
                    "error": "notification_only watch requires a matching merge receipt (repo + head_sha + task_id)",
                    "code": "notification_only_no_receipt",
                });
            };
            if receipt.task_assignee != instance_name {
                return json!({
                    "error": format!(
                        "notification_only watch: caller '{}' is not the task assignee '{}'",
                        instance_name, receipt.task_assignee
                    ),
                    "code": "notification_only_unauthorized",
                });
            }
            {
                let binding = crate::binding::read(home, instance_name);
                let bound_task = binding
                    .as_ref()
                    .and_then(|b| b["task_id"].as_str())
                    .unwrap_or("");
                if bound_task != task_id {
                    return json!({
                        "error": format!(
                            "notification_only watch: caller binding task_id '{bound_task}' does not match watch task_id '{task_id}'"
                        ),
                        "code": "notification_only_binding_mismatch",
                    });
                }
            }
            // Passes all guards — fall through to arm the watch below.
        } else if !has_task_id || next_targets.is_empty() {
            return json!({
                "error": "exact-head protected watch requires BOTH `task_id` and an explicit `next_after_ci` target",
                "code": "protected_watch_missing_requirements",
            });
        } else {
            // Privileged orchestrator/operator path (unchanged).
            let authorized = instance_name.is_empty()
                || next_targets
                    .iter()
                    .all(|m| crate::teams::is_orchestrator_of(home, instance_name, m));
            if !authorized {
                return json!({
                    "error": format!("'{instance_name}' may not arm a protected-branch exact-head watch — only the target team orchestrator or operator may"),
                    "code": "protected_watch_unauthorized",
                });
            }
        }
        // By-SHA resolution is GitHub-only this wave — fail loud rather than arm a
        // watch the poller could never resolve.
        let (provider_kind, _) = crate::daemon::ci_watch::detect_provider_from_remote(repo);
        if provider_kind != "github" {
            return json!({
                "error": format!("exact-head protected watch is GitHub-only this wave (detected provider: {provider_kind})"),
                "code": "protected_watch_provider_unsupported",
            });
        }
        Some(crate::daemon::ci_watch::normalize_head_sha(head_sha))
    } else {
        None
    };

    // Reject unsupported providers early with operator-actionable error.
    if args["ci_provider"].as_str() == Some("bitbucket_server") {
        return json!({"error": "Bitbucket Server not yet supported — track Sprint 41+ candidate. Use bitbucket_cloud for Bitbucket Cloud repos."});
    }

    // A task-linked watch must use the task's freshly revalidated authority.
    // Derive the requested class before creating or mutating any watch/PrState
    // file, so an omitted class inherits safely and a mismatch fails closed.
    let requested_review_class: Option<String> = if let Some(task_id) =
        args["task_id"].as_str().filter(|id| !id.is_empty())
    {
        match crate::tasks::load_routed(home, task_id) {
            Ok(routed) => {
                let task_class = match crate::tasks::governance::validate_existing_authority(
                    home,
                    task_id,
                    routed.record(),
                ) {
                    Ok(Some(class)) => Some(class),
                    Ok(None) => {
                        // Preserve the pre-#3419 task-linked watch contract for
                        // ungoverned legacy tasks that never carried a typed
                        // class. A governed task (or an explicit class on a
                        // classless task) still has no durable authority and
                        // must fail closed.
                        let has_explicit_class = args
                            .get("review_class")
                            .is_some_and(|value| !value.is_null());
                        if routed
                            .record()
                            .metadata
                            .contains_key("governing_decision_id")
                            || has_explicit_class
                        {
                            return json!({
                                "error": "task-linked CI watch requires a resolved task review_class",
                                "code": "watch_review_class_unspecified",
                                "task_id": task_id,
                            });
                        }
                        None
                    }
                    Err(error) => return error,
                };
                if let Some(value) = args.get("review_class").filter(|value| !value.is_null()) {
                    let Some(raw) = value.as_str().filter(|raw| !raw.is_empty()) else {
                        return json!({
                            "error": "task-linked CI watch review_class must be exactly 'single' or 'dual'",
                            "code": "watch_review_class_invalid",
                            "task_id": task_id,
                        });
                    };
                    let Some(task_class) = task_class else {
                        return json!({
                            "error": "task-linked CI watch requires a resolved task review_class",
                            "code": "watch_review_class_unspecified",
                            "task_id": task_id,
                        });
                    };
                    if crate::daemon::pr_state::ReviewClass::parse_fail_closed(Some(raw))
                        != task_class
                    {
                        return json!({
                            "error": format!(
                                "task review_class={} conflicts with CI watch review_class",
                                task_class.as_token()
                            ),
                            "code": "watch_review_class_mismatch",
                            "task_id": task_id,
                        });
                    }
                }
                task_class.map(|class| class.as_token().to_string())
            }
            // A task id that routes to NO board carries no authority, so an
            // identified caller must not supply the class itself — that was the
            // fail-open here: any agent could name a phantom task and stamp its
            // own class (e.g. `single` onto a dual-required subject).
            //
            // The transient the previous comment invoked ("record not yet
            // available") cannot reach this arm: `route_task` resolves through
            // the cross-board catalog, whose `route_with_revision` calls
            // `ensure_fresh()` and reports a not-yet-Ready board as `Unreadable`
            // — handled by the reject arm below. `NotFound` here means the task
            // exists nowhere.
            Err(crate::tasks::TaskRouteError::NotFound) => {
                let explicit_class = args["review_class"]
                    .as_str()
                    .filter(|class| !class.is_empty())
                    .map(String::from);
                match (instance_name.is_empty(), explicit_class) {
                    // The fail-open this arm used to be: an identified caller
                    // names a task that routes to NO board and stamps its own
                    // class (e.g. `single` onto a dual-required subject). The
                    // task carries no authority, so neither does the caller.
                    (false, Some(_)) => {
                        return json!({
                            "error": "task-linked CI watch names a task that routes to no board",
                            "code": "watch_task_link_unresolved",
                            "task_id": task_id,
                        })
                    }
                    // Deliberately still allowed: with no class supplied nothing
                    // is asserted — `review_class` is written below only when
                    // `Some`, so the sidecar carries none and the merge gate
                    // still fails closed on the PR state's own Unresolved class.
                    // Narrowing the gate to the stamping case keeps it surgical.
                    (false, None) => None,
                    // Operator override (empty caller) keeps its explicit class,
                    // but never silently: record who retained which class for
                    // which task.
                    (true, Some(class)) => {
                        crate::event_log::log(
                            home,
                            "ci_watch_taskless_class_override",
                            instance_name,
                            &format!("task_id={task_id} review_class={class}"),
                        );
                        Some(class)
                    }
                    (true, None) => None,
                }
            }
            Err(error) => {
                return json!({
                    "error": format!("task-linked CI watch route is unresolved: {error}"),
                    "code": "watch_task_route_unresolved",
                    "task_id": task_id,
                })
            }
        }
    } else {
        args["review_class"]
            .as_str()
            .filter(|class| !class.is_empty())
            .map(String::from)
    };

    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    // #779 P2 Piece 3 site A: surface a dir-create failure as a structured
    // `{error, code}` (pre-#779-P2 swallowed it and returned happy-path even when
    // the subsequent atomic_write was doomed).
    if let Err(e) = std::fs::create_dir_all(&ci_dir) {
        return json!({
            "error": format!("ci-watches dir create failed: {e}"),
            "code": "ci_watches_dir_create_failed",
        });
    }
    // Exact-head protected watches key on repo:branch:head_sha so they never
    // collide with a generic branch watch and multiple post-merge SHAs coexist.
    let filename = match exact_head_sha.as_deref() {
        Some(sha) => crate::daemon::ci_watch::watch_filename_exact_head(repo, branch, sha),
        None => crate::daemon::ci_watch::watch_filename(repo, branch),
    };
    let watch_path = ci_dir.join(&filename);

    // H5 (CR-2026-06-14): flock the read→mutate→atomic_write window (mirrors
    // registry.rs / the poll loop). atomic_write makes each write atomic but not
    // the read→write gap, so an unlocked MCP RMW loses poll-state/subscriber
    // updates racing a concurrent poll/unwatch.
    let _watch_lock = crate::store::acquire_file_lock(&watch_path.with_extension("lock"));

    let now_rfc3339 = chrono::Utc::now().to_rfc3339();

    let existing = std::fs::read_to_string(&watch_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    // Re-arming an unwatch TOMBSTONE? Same predicate as `sweep.rs`
    // (`auto_arm_optout` + no subscribers), read from the PRE-EXISTING file —
    // before this call appends its subscriber or removes the optout below.
    let tombstone_rearm = existing.as_ref().is_some_and(|w| {
        w.get("auto_arm_optout").and_then(|v| v.as_bool()) == Some(true)
            && crate::daemon::ci_watch::parse_subscribers(w).is_empty()
    });
    let mut watch = existing.unwrap_or_else(|| {
            json!({
                "repo": repo,
                "branch": branch,
                "interval_secs": interval,
                "ci_provider": args["ci_provider"].as_str(),
                "ci_provider_url": args["ci_provider_url"].as_str(),
                "last_run_id": null,
                "head_sha": null,
                "last_polled_at": null,
                "last_notified_head_sha": null,
                "expires_at": (chrono::Utc::now() + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS)).to_rfc3339(),
                "last_terminal_seen_at": null,
                "generation_id": uuid::Uuid::new_v4().to_string(),
            })
        });
    // Seed legacy watches missing generation_id.
    if watch
        .get("generation_id")
        .and_then(|v| v.as_str())
        .is_none()
    {
        watch["generation_id"] = json!(uuid::Uuid::new_v4().to_string());
    }

    // Migrate legacy schema (single `instance` field, no `subscribers`
    // array) into the canonical multi-subscriber form. Subsequent reads
    // by the daemon's poll loop go through `parse_subscribers` which
    // also supports the legacy form, so a migration race here is safe.
    let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
    if !subscribers.iter().any(|s| s == instance_name) && !instance_name.is_empty() {
        subscribers.push(instance_name.to_string());
    }
    // Post-merge exact-head watches are armed by an anonymous privileged
    // caller. Make their explicit handoff targets durable poll subscribers;
    // named callers and feature-branch watches retain their existing behavior.
    let add_exact_head_handoff_subscribers = exact_head_sha.is_some()
        && instance_name.is_empty()
        && !args["notification_only"].as_bool().unwrap_or(false);
    if add_exact_head_handoff_subscribers {
        let targets = crate::daemon::ci_watch::watch_state::normalize_next_after_ci(
            args.get("next_after_ci").unwrap_or(&Value::Null),
        );
        for target in targets {
            if !subscribers.iter().any(|subscriber| subscriber == &target) {
                subscribers.push(target);
            }
        }
    }
    let subscribers_json: Vec<Value> = subscribers
        .iter()
        .map(|name| {
            // Preserve original subscribed_at if present, otherwise stamp now.
            let prior = watch
                .get("subscribers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|s| s.get("instance").and_then(|i| i.as_str()) == Some(name.as_str()))
                })
                .and_then(|s| s.get("subscribed_at").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| now_rfc3339.clone());
            json!({"instance": name, "subscribed_at": prior})
        })
        .collect();

    watch["repo"] = json!(repo);
    watch["branch"] = json!(branch);
    // Refresh interval / provider override on each call — caller may
    // adjust polling cadence or provider URL even on a re-subscribe.
    watch["interval_secs"] = json!(interval);
    if let Some(p) = args["ci_provider"].as_str() {
        watch["ci_provider"] = json!(p);
    }
    if let Some(u) = args["ci_provider_url"].as_str() {
        // AUDIT2-001: the daemon will refuse to send the forge token to an
        // untrusted host. Surface that once, at subscribe time, so a legitimate
        // self-hosted GHE/GitLab operator knows to allowlist the host rather
        // than silently polling unauthenticated.
        if !u.is_empty() && !crate::daemon::ci_watch::host_receives_credentials(u) {
            tracing::warn!(
                ci_provider_url = %u,
                "ci watch: ci_provider_url host is not in the CI trusted-host \
                 allowlist; the forge token will NOT be sent to it (prevents \
                 token exfiltration). Set AGEND_CI_TRUSTED_HOSTS=<host> to allow \
                 a self-hosted GHE/GitLab host."
            );
        }
        watch["ci_provider_url"] = json!(u);
    }
    watch["subscribers"] = json!(subscribers_json);
    // DEPRECATED: legacy alias; post-r0 daemons read `subscribers`.
    watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());
    // #1991: an explicit (re-)watch overrides a prior unwatch tombstone —
    // the human/agent decision to watch again clears the auto-arm optout.
    if let Some(obj) = watch.as_object_mut() {
        obj.remove("auto_arm_optout");
        // A tombstone re-arm starts a fresh notification epoch: these cursors
        // record what the previous epoch's (now gone) subscribers were already
        // told, so keeping them suppresses a still-terminal run at the same
        // head. Rotating `generation_id` in this SAME write makes an in-flight
        // old-generation poll lose the `flush_watch_state` CAS instead of
        // restoring them. Keys are removed, not nulled: every field is
        // `#[serde(default)]` and `last_notified_by_workflow` is a plain
        // `BTreeMap`, so a null would make the watch unreadable to the poller.
        if tombstone_rearm {
            for key in [
                "last_notified_by_workflow",
                "last_run_id",
                "last_notified_head_sha",
                "last_notified_conclusion",
                "last_notified_run_attempt",
                "last_notified_run_conclusion",
                "last_terminal_seen_at",
                "terminal_since",
            ] {
                obj.remove(key);
            }
            obj.insert(
                "generation_id".to_string(),
                json!(uuid::Uuid::new_v4().to_string()),
            );
        }
    }
    // Refresh expires_at on each subscribe — keeps the watch alive
    // as long as at least one agent stays interested.
    watch["expires_at"] = json!((chrono::Utc::now()
        + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS))
    .to_rfc3339());
    // Issue #650 + CR-2026-06-14: set on non-empty; explicit empty CLEARS the
    // stale handoff (re-arm with no chaining); absent leaves it untouched.
    if let Some(next_arg) = args.get("next_after_ci") {
        let targets = crate::daemon::ci_watch::watch_state::normalize_next_after_ci(next_arg);
        if let Some(next_json) = crate::daemon::ci_watch::watch_state::next_after_ci_json(&targets)
        {
            watch["next_after_ci"] = next_json;
        } else {
            if let Some(obj) = watch.as_object_mut() {
                obj.remove("next_after_ci");
            }
        }
    }
    // #1031: persist dispatch task_id as structured back-link.
    if let Some(tid) = args["task_id"].as_str().filter(|s| !s.is_empty()) {
        watch["task_id"] = json!(tid);
    }
    // #972: persist review_class for §3.5 dual-review gate.
    if let Some(rc) = requested_review_class.as_deref() {
        watch["review_class"] = json!(rc);
    }
    // S1: persist the (validated, lowercased) exact-head pin. Its PRESENCE marks
    // this as a protected post-merge watch the poller resolves by target SHA and
    // `gc_stale_watches` preserves across restart. Only reachable here after the
    // exact-head gate above, so a non-protected watch never carries it.
    if let Some(sha) = exact_head_sha.as_deref() {
        watch["target_head_sha"] = json!(sha);
    }
    // #2812: notification-only watch — short TTL (1h), persisted flag.
    // Only valid on protected refs (the gate above validates all guards).
    let notification_only = args["notification_only"].as_bool().unwrap_or(false);
    if notification_only {
        if exact_head_sha.is_none() {
            return json!({
                "error": "notification_only watch is only valid on protected refs with an exact head_sha",
                "code": "notification_only_non_protected",
            });
        }
        watch["notification_only"] = json!(true);
        watch["next_after_ci"] = json!(null);
        let short_ttl = chrono::Utc::now()
            + chrono::TimeDelta::try_hours(1).unwrap_or(chrono::TimeDelta::zero());
        watch["expires_at"] = json!(short_ttl.to_rfc3339());
    } else if let Some(obj) = watch.as_object_mut() {
        obj.remove("notification_only");
    }

    // #779 P2 Piece 3 site B: atomic_write failure (disk full,
    // permission, etc.) previously surfaced as `let _ = ...` silent
    // discard, returning happy-path Value with `watching: true` even
    // when the watch file was never written. Now surface as structured
    // error so callers don't act on phantom state. NOTE: site C
    // (line ~362 `read_to_string(&watch_path).ok()`) is intentionally
    // NOT hardened — its None case is the load-bearing fresh-watch
    // init path; hardening there would block legitimate first
    // subscribes.
    if let Err(e) = crate::store::atomic_write(
        &watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        return json!({
            "error": format!("watch file write failed: {e}"),
            "code": "watch_write_failed",
        });
    }
    // #3114: an explicit class can repair a persisted Unresolved PR gate even
    // when CI already notified at this head. Reconcile the gate directly rather
    // than clearing CI cursors, which would replay ci-pass/ci-ready messages.
    // Run this on every explicit class so a partial prior failure is retryable.
    if let Some(requested) = requested_review_class.as_deref() {
        let requested = crate::daemon::pr_state::ReviewClass::parse_fail_closed(Some(requested));
        if let Err(e) = crate::daemon::pr_state::with_pr_state(home, repo, branch, |state| {
            let was_unresolved = matches!(
                state.review_class,
                crate::daemon::pr_state::ReviewClass::Unresolved
            );
            state.review_class =
                crate::daemon::pr_state::reconcile_review_class(state.review_class, requested);
            if was_unresolved
                && !matches!(
                    state.review_class,
                    crate::daemon::pr_state::ReviewClass::Unresolved
                )
            {
                state.diagnostic_emitted_for_sha = None;
            }
            if !matches!(
                state.merge_state,
                crate::daemon::pr_state::MergeState::Merged { .. }
                    | crate::daemon::pr_state::MergeState::ClosedUnmerged { .. }
            ) {
                state.merge_state = if crate::daemon::pr_state::is_merge_ready(state) {
                    crate::daemon::pr_state::MergeState::MergeReady
                } else {
                    crate::daemon::pr_state::MergeState::NotReady
                };
            }
        }) {
            return json!({
                "error": format!("review class reconciliation failed: {e}"),
                "code": "review_class_reconcile_failed",
            });
        }
    }
    // #813: on-watch-start mergeable check. Builds a default provider
    // for the repo (GitHub-only impl; GitLab/Bitbucket inherit the
    // Unknown stub per §3.7), queries mergeable_state synchronously,
    // and emits `[ci-conflict-detected]` to every subscriber if the
    // PR is in DIRTY state. Fail-open on any provider error.
    let subscribers_for_alert: Vec<String> = crate::daemon::ci_watch::parse_subscribers(&watch);
    if let Some(provider) = super::build_default_provider(repo) {
        crate::daemon::ci_watch::watch_start_check_mergeable(
            home,
            &watch_path,
            repo,
            branch,
            &subscribers_for_alert,
            provider.as_ref(),
        );
    }
    // Sprint 54 P0-5 (sub-scope A): response enrichment — agents see
    // CI health without polling the watch file. Read state freshly
    // from `watch` JSON we just composed; populate diagnostic fields
    // when the data is available, leave as `null` otherwise.
    let now_secs = chrono::Utc::now().timestamp();
    let rate_limit_until = watch["rate_limit_until"].as_i64();
    let rate_limit_active = match rate_limit_until {
        Some(reset) => reset > now_secs,
        None => false,
    };
    let next_poll_eta = super::compute_next_poll_eta(&watch);

    let mut resp = json!({
        "repo": repo,
        "watching": true,
        "subscribers": subscribers,
        "rate_limit_active": rate_limit_active,
        "rate_limit_until": rate_limit_until,
        "next_poll_eta": next_poll_eta,
    });
    // Sprint 54 P0-4: surface `setup_warning` (canonical field name per
    // FLEET-DEV-PROTOCOL §X) so agents can advise users to install
    // `gh` or set `GITHUB_TOKEN`. Only fires when neither env nor
    // `gh auth` produced a token.
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

/// `ci status` — snapshot of CI watches the caller subscribes to.
pub(crate) fn handle_status_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let filter_repo = args["repository"].as_str();
    let filter_branch = args["branch"].as_str();
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    // #35896-11 ③: do NOT early-return on an absent/empty ci-watches dir — a live
    // ci_handoff_track (a SEPARATE `ci-handoff-tracks` dir) can outlast its watch
    // (unwatched/expired watch, live renudge), which is EXACTLY lead's 4.5h sample
    // (empty `watches`, silent renudge). The pending_handoffs surface below must
    // still render, so fall through with zero watch rows. `into_iter().flatten()`
    // yields nothing when the dir is missing → `out` stays empty.
    let entries = std::fs::read_dir(&ci_dir).into_iter().flatten();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_secs = chrono::Utc::now().timestamp();

    let mut out: Vec<Value> = Vec::new();
    // t-…-67: watches on the requested filter the caller cannot see (`status_scope`).
    let mut hidden_watches: usize = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let repo = match watch["repo"].as_str() {
            Some(r) => r,
            None => continue,
        };
        let branch = watch["branch"].as_str().unwrap_or("main");
        if let Some(want) = filter_repo {
            if repo != want {
                continue;
            }
        }
        if let Some(want) = filter_branch {
            if branch != want {
                continue;
            }
        }
        let subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
        // Caller scoping: an agent with a name sees only the watches
        // they're a subscriber of. Anonymous calls (empty instance)
        // see everything — useful for operator triage via the CLI.
        if !instance_name.is_empty() && !subscribers.iter().any(|s| s == instance_name) {
            hidden_watches += 1;
            continue;
        }
        let rate_limit_until = watch["rate_limit_until"].as_i64();
        let rate_limit_active = match rate_limit_until {
            Some(reset) => reset > now_secs,
            None => false,
        };
        let _ = now_ms; // anchor: keep timestamp-millis consistency with response enrichment
        out.push(json!({
            "repo": repo,
            "branch": branch,
            "subscribers": subscribers,
            "rate_limit_active": rate_limit_active,
            "rate_limit_until": rate_limit_until,
            "rate_limit_remaining": watch["rate_limit_remaining"].as_u64(),
            "rate_limit_limit": watch["rate_limit_limit"].as_u64(),
            "effective_interval_secs": watch["effective_interval_secs"].as_u64(),
            "interval_secs": watch["interval_secs"].as_u64().unwrap_or(60),
            "next_poll_eta": super::compute_next_poll_eta(&watch),
            "consecutive_skips": watch["consecutive_skips"].as_u64().unwrap_or(0),
            "stalled_notified": watch["stalled_notified"].as_bool().unwrap_or(false),
            "stalled_since_ms": watch["stalled_since_ms"].as_i64(),
            "last_polled_at": watch["last_polled_at"].as_i64(),
            "last_terminal_seen_at": watch["last_terminal_seen_at"].as_str(),
            "head_sha": watch["head_sha"].as_str(),
            "target_head_sha": watch["target_head_sha"].as_str(),
            "expires_at": watch["expires_at"].as_str(),
            // #813: surface cached mergeable state so callers can
            // distinguish "CI running" silence from "CONFLICTING
            // blocked forever" silence. Field is `null` for watches
            // that haven't run their first mergeable check yet.
            "pr_mergeable_state": watch["last_mergeable_state"].as_str(),
            "pr_mergeable_check_at": watch["last_mergeable_check_at"].as_str(),
            // #1473 display gap: surface the stored CI-pass handoff target so
            // `ci action=status` shows it (previously omitted → operators
            // mis-read it as unset even when armed).
            "next_after_ci": watch.get("next_after_ci").cloned().unwrap_or(Value::Null),
        }));
    }
    // #35896-11 ③: surface pending ci_handoff_track sidecars so an agent can SEE
    // why the ci-ready renudge watchdog keeps nudging them and what to discharge.
    // Before this the renudge had NO status surface (lead's 4.5h sample: `ci
    // status` showed empty `watches` the whole time the sidecar-driven renudge
    // fired every 2min — invisible, no discharge target). Caller-scoped to the
    // track TARGET (who owes the review = who gets renudged), mirroring the watch
    // caller-scoping above; the anonymous CLI (empty instance) sees all. The
    // optional `repository`/`branch` args narrow it the same way they narrow
    // watches. `renudge_count` is intentionally absent — the throttle counter is
    // not persisted on the track yet (#35896-11 ⑥, PR-C); `age_secs` is the
    // renudge driver and IS surfaced so an agent can gauge staleness.
    let pending_handoffs: Vec<Value> = crate::daemon::ci_handoff_track::list(home)
        .into_iter()
        .map(|(_, t)| t)
        .filter(|t| instance_name.is_empty() || t.target == instance_name)
        .filter(|t| filter_repo.is_none_or(|r| t.correlation.split('@').next() == Some(r)))
        .filter(|t| filter_branch.is_none_or(|b| t.correlation.ends_with(&format!("@{b}"))))
        .map(|t| {
            let age_secs = chrono::DateTime::parse_from_rfc3339(&t.sent_at)
                .ok()
                .map(|s| now_secs - s.timestamp());
            let message_id: Option<String> = t.ci_handoff_episode.as_deref().and_then(|ep| {
                let inbox_path = crate::inbox::inbox_path_resolved(home, &t.target);
                let content = std::fs::read_to_string(&inbox_path).ok()?;
                let corr = &t.correlation;
                let matches: Vec<String> = content
                    .lines()
                    .filter_map(|line| {
                        serde_json::from_str::<crate::inbox::InboxMessage>(line).ok()
                    })
                    .filter(|m| {
                        m.kind.as_deref() == Some("ci-ready-for-action")
                            && m.ci_handoff_episode.as_deref() == Some(ep)
                            && m.correlation_id.as_deref() == Some(corr)
                    })
                    .filter_map(|m| m.id.clone())
                    .collect();
                if matches.len() == 1 {
                    Some(matches.into_iter().next()?)
                } else {
                    None
                }
            });
            json!({
                "target": t.target,
                "correlation": t.correlation,
                "task_id": t.task_id,
                "head_sha": t.head_sha,
                "sent_at": t.sent_at,
                "age_secs": age_secs,
                "episode": t.ci_handoff_episode,
                "class": t.ci_handoff_class,
                "state": if t.is_deferred() { "deferred" } else { "active" },
                "wake_task_id": t.wake_task_id,
                "defer_expires_at": t.defer_expires_at,
                "defer_reason": t.defer_reason,
                "message_id": message_id,
            })
        })
        .collect();
    // t-…-67: the view names its scope and what it hid — see `status_scope`.
    let mut resp = json!({
        "watches": out,
        "pending_handoffs": pending_handoffs,
        "scope": super::status_scope::scope_label(instance_name),
        "hidden_watches": hidden_watches,
    });
    if hidden_watches > 0 {
        let filtered = filter_repo.is_some() || filter_branch.is_some();
        resp["hint"] = json!(super::status_scope::hidden_hint(hidden_watches, filtered));
    }
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

/// #813: build the default `CiProvider` for a repo URL. Mirrors
#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "watch_tests.rs"]
mod tests;
