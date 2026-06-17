use serde_json::{json, Value};
use std::path::Path;

/// `ci watch` action: APPEND-idempotently subscribe `instance_name` to CI
/// notifications for `repo@branch` (Sprint 54 P0-1: preserves other agents'
/// subscriptions + existing poll state, vs the prior last-write-wins overwrite).
pub(crate) fn handle_watch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    // Sprint 55 P0-B: when caller omits `repo` arg, auto-derive from
    // sender's binding.json source_repo (set by `bind_self` /
    // `dispatch_auto_bind_lease`). #1619: shared `resolve_repo_or_error`
    // — explicit error when neither arg nor binding present (no silent
    // cwd-derivation, no hardcoded fallback).
    let repo_owned = match super::resolve_repo_or_error(home, instance_name, args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let repo: &str = &repo_owned;
    let branch = args["branch"].as_str().unwrap_or("main");
    let interval = args["interval_secs"].as_u64().unwrap_or(60);

    // Sprint 57 Wave 2 Track B (#546 Item 3) — E4.5 protected-ref
    // gate. Closes the bypass that let any agent subscribe to `main`
    // (or `master`) CI by calling `ci action=watch` directly. Mirrors
    // the worktree-lease gate in `worktree_pool::lease`; both go
    // through `agent_ops::is_protected_ref` so the protected set is
    // edited in exactly one place. The "main" default at the line
    // above is the backstop the gate catches when callers omit both
    // `branch` and explicit-protected branch — both flows land here.
    if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
        return e;
    }

    // Reject unsupported providers early with operator-actionable error.
    if args["ci_provider"].as_str() == Some("bitbucket_server") {
        return json!({"error": "Bitbucket Server not yet supported — track Sprint 41+ candidate. Use bitbucket_cloud for Bitbucket Cloud repos."});
    }

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
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let watch_path = ci_dir.join(&filename);

    // H5 (CR-2026-06-14): flock the read→mutate→atomic_write window (mirrors
    // registry.rs / the poll loop). atomic_write makes each write atomic but not
    // the read→write gap, so an unlocked MCP RMW loses poll-state/subscriber
    // updates racing a concurrent poll/unwatch.
    let _watch_lock = crate::store::acquire_file_lock(&watch_path.with_extension("lock"));

    let now_rfc3339 = chrono::Utc::now().to_rfc3339();

    let mut watch = std::fs::read_to_string(&watch_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| {
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
            })
        });

    // Migrate legacy schema (single `instance` field, no `subscribers`
    // array) into the canonical multi-subscriber form. Subsequent reads
    // by the daemon's poll loop go through `parse_subscribers` which
    // also supports the legacy form, so a migration race here is safe.
    let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
    if !subscribers.iter().any(|s| s == instance_name) && !instance_name.is_empty() {
        subscribers.push(instance_name.to_string());
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
        watch["ci_provider_url"] = json!(u);
    }
    watch["subscribers"] = json!(subscribers_json);
    // DEPRECATED: `instance` field kept as legacy alias for one release
    // cycle so a daemon running pre-r0 binary against post-r0 watch
    // files can still read SOMEONE. Set to first subscriber, removed
    // Sprint 55. Post-r0 daemons read `subscribers` first.
    watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());
    // #1991: an explicit (re-)watch overrides a prior unwatch tombstone —
    // the human/agent decision to watch again clears the auto-arm optout.
    if let Some(obj) = watch.as_object_mut() {
        obj.remove("auto_arm_optout");
    }
    // Refresh expires_at on each subscribe — keeps the watch alive
    // as long as at least one agent stays interested.
    watch["expires_at"] = json!((chrono::Utc::now()
        + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS))
    .to_rfc3339());
    // Issue #650 + CR-2026-06-14: set on non-empty; explicit empty CLEARS the
    // stale handoff (re-arm with no chaining); absent leaves it untouched.
    match args.get("next_after_ci").and_then(|v| v.as_str()) {
        Some(next) if !next.is_empty() => watch["next_after_ci"] = json!(next),
        Some(_) => {
            if let Some(obj) = watch.as_object_mut() {
                obj.remove("next_after_ci");
            }
        }
        None => {}
    }
    // #1031: persist dispatch task_id when supplied (by
    // dispatch_auto_bind_lease) so the ci_check_repo emit site can
    // populate `[ci-ready-for-action]` InboxMessage's task_id field,
    // giving the reviewer a structured back-link to the originating
    // dispatch. Manual `ci action=watch` callers may also pass
    // task_id explicitly to bind the watch to a specific task.
    if let Some(tid) = args["task_id"].as_str().filter(|s| !s.is_empty()) {
        watch["task_id"] = json!(tid);
    }
    // #972 reviewer-rejection fix: persist `review_class` so the
    // pr_state aggregator can honor §3.5 dual-review at runtime. Accepted
    // values: `"single"` (default — §3.6) or `"dual"` (§3.5). Other
    // strings are tolerated and treated as Single at read time
    // (see `daemon::ci_watch::poller::parse_review_class`). Without
    // this field operator must currently `delete fleet.yaml` to
    // remove the watch and re-arm with `--review-class dual` —
    // documented as a workflow gap to close in a follow-up CLI/MCP
    // exposure.
    if let Some(rc) = args["review_class"].as_str().filter(|s| !s.is_empty()) {
        watch["review_class"] = json!(rc);
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
    // #813: on-watch-start mergeable check. Builds a default provider
    // for the repo (GitHub-only impl; GitLab/Bitbucket inherit the
    // Unknown stub per §3.7), queries mergeable_state synchronously,
    // and emits `[ci-conflict-detected]` to every subscriber if the
    // PR is in DIRTY state. Fail-open on any provider error.
    let subscribers_for_alert: Vec<String> = crate::daemon::ci_watch::parse_subscribers(&watch);
    if let Some(provider) = build_default_provider(repo) {
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

/// `ci unwatch` action: unsubscribe the caller from `repo@branch`.
/// Sprint 54 P0-1: only the caller is removed from the `subscribers`
/// array. The watch file is deleted only when the array becomes empty
/// (no other agent is still interested in this branch).
// pub(crate): the #1991 auto-arm tombstone regression test (pr_state::auto_arm)
// exercises the real unwatch → tombstone → no-re-arm chain.
pub(crate) fn handle_unwatch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repo = match args["repository"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repository'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    // Caller identity for selective removal (H4, CR-2026-06-14): the VALIDATED
    // sender when `instance` arg is absent/empty (`.filter`, mirroring the
    // sibling cleanup handlers) — NOT a daemon `std::env::var` read (was EMPTY in
    // the daemon → the empty-caller `subscribers.clear()` path below).
    let caller = args["instance"]
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| instance_name.to_string());
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
            return json!({"repo": repo, "watching": false, "subscribers": Vec::<String>::new()});
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
            "watching": false,
            "subscribers": Vec::<String>::new(),
            "tombstone": true,
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
        "watching": true,
        "subscribers": subscribers,
    })
}

/// `ci status` MCP action (Sprint 54 P0-5 sub-scope C). Returns a
/// snapshot of every CI watch the caller subscribes to, with full
/// health diagnostics inlined. Optional `repo` / `branch` args narrow
/// the result; both must match when both are provided.
///
/// Caller filtering: agents only see watches they're subscribed to —
/// avoids leaking lead's polling targets to every dev. The empty
/// instance name (anonymous CLI) sees all watches.
pub(crate) fn handle_status_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let filter_repo = args["repository"].as_str();
    let filter_branch = args["branch"].as_str();
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&ci_dir) {
        Ok(e) => e,
        Err(_) => return json!({"watches": Vec::<Value>::new()}),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_secs = chrono::Utc::now().timestamp();

    let mut out: Vec<Value> = Vec::new();
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
            "next_after_ci": watch["next_after_ci"].as_str(),
        }));
    }
    let mut resp = json!({"watches": out});
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

/// #813: build the default `CiProvider` for a repo URL. Mirrors
/// `watcher.rs::check_ci_watches`'s factory but with the canonical
/// host URLs (no per-watch URL override) — sufficient for the
/// on-watch-start mergeable check at dispatch time. GitHub fully
/// implemented; GitLab/Bitbucket return Unknown via the trait
/// default (§3.7 cross-backend stance — promotion blocked behind
/// real operator usage).
fn build_default_provider(repo: &str) -> Option<Box<dyn crate::daemon::ci_watch::CiProvider>> {
    use crate::daemon::ci_watch::{
        detect_provider_from_remote, BitbucketCiProvider, CiProvider, GitHubCiProvider,
        GitLabCiProvider,
    };
    let (kind, _is_custom) = detect_provider_from_remote(repo);
    let provider: Option<Box<dyn CiProvider>> = match kind {
        "gitlab" => GitLabCiProvider::with_base_url("https://gitlab.com".to_string())
            .ok()
            .map(|p| Box::new(p) as Box<dyn CiProvider>),
        "bitbucket_cloud" => {
            BitbucketCiProvider::with_base_url("https://api.bitbucket.org".to_string())
                .ok()
                .map(|p| Box::new(p) as Box<dyn CiProvider>)
        }
        _ => GitHubCiProvider::with_base_url("https://api.github.com".to_string())
            .ok()
            .map(|p| Box::new(p) as Box<dyn CiProvider>),
    };
    provider
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #t-92758 P2: `ci unwatch` is the lead's dismiss path for a stuck ci-ready —
    /// it must clear the caller's own ci-handoff track so the re-nudge watchdog
    /// stops. Runs even when no watch file exists (the dismiss intent stands).
    #[test]
    fn unwatch_resolves_callers_ci_handoff_track() {
        let home = std::env::temp_dir().join(format!(
            "agend-92758-unwatch-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        // A ci-ready obligation pointing at the caller, plus a co-subscriber's
        // track on the same branch that must survive (precise dismiss).
        crate::daemon::ci_handoff_track::record(
            &home,
            "lead",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
        );
        crate::daemon::ci_handoff_track::record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
        );

        let args = json!({"repository": "o/r", "branch": "b", "instance": "lead"});
        let _ = handle_unwatch_ci(&home, &args, "lead");

        let left = crate::daemon::ci_handoff_track::list(&home);
        assert_eq!(left.len(), 1, "only the caller's track is cleared");
        assert_eq!(
            left[0].1.target, "reviewer",
            "co-subscriber's track must survive unwatch dismiss"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
