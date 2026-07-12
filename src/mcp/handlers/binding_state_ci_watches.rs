//! S2: ci-watch enumeration for `binding_state` — the byte-for-byte `ci_watches`
//! string list + the additive `ci_watches_detail` projection. Extracted from
//! binding_state.rs to keep that file under the MCP-handler LOC ceiling.

use serde_json::{json, Value};
use std::path::Path;

/// Subscriber-scoped `"<repo>:<branch>"` strings, sorted. UNCHANGED byte-for-byte
/// from the pre-S2 inline version (`ci_watches` back-compat).
pub(super) fn enumerate_ci_watches_for_agent(home: &Path, agent: &str) -> Vec<String> {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let subs = crate::daemon::ci_watch::parse_subscribers(&watch);
        if subs.iter().any(|s| s == agent) {
            let repo = watch["repo"].as_str().unwrap_or("?");
            let branch = watch["branch"].as_str().unwrap_or("?");
            out.push(format!("{repo}:{branch}"));
        }
    }
    out.sort();
    out
}

/// S2 additive detail — one row per watch the agent subscribes to.
///
/// `current_repo`/`current_branch` are the agent's CURRENT binding identity (the
/// owner/repo slug derived from its binding `source_repo` + branch). A watch is
/// `current_binding` only when BOTH match — binding identity is repo+branch, not
/// branch alone: a same-branch watch on a DIFFERENT repo is NOT current. Empty
/// `current_repo` (unbound, or a non-derivable remote) ⇒ nothing is current.
///
/// Rows GC's protected-migration arm would remove — a protected-ref watch
/// (`main`/`master`) that is NOT a valid exact-head (absent/malformed target SHA)
/// — are EXCLUDED (pending deletion, out of per-agent scope; mirrors the GC order
/// where protected-migration precedes the TTL classifier). A VALID exact-head
/// protected watch is kept. `lifecycle`/`expiry_reason` come from the SAME
/// `classify_subscribed_watch_expiry` the GC reaps on (no drift). A non-current
/// but not-reap-eligible watch is `polling`, NOT stale (#931). Tombstones never
/// appear (subscriber-scoped). Sorted by (repo, branch, target_head_sha).
pub(super) fn enumerate_ci_watches_detail_for_agent(
    home: &Path,
    agent: &str,
    current_repo: &str,
    current_branch: &str,
) -> Vec<Value> {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return Vec::new();
    };
    let now_utc = chrono::Utc::now();
    let mut rows: Vec<(String, String, String, Value)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<crate::daemon::ci_watch::WatchState>(&content)
        else {
            continue;
        };
        if !watch.subscriber_names().iter().any(|s| s == agent) {
            continue;
        }
        let repo = watch.repo.clone();
        let branch = watch.branch.clone();
        let target_head_sha = watch.target_head_sha.clone();
        // GC removes a protected-ref watch that is NOT a valid exact-head BEFORE
        // the TTL classifier (protected-migration). Such a row is pending deletion
        // — exclude it so binding_state never reports it as `polling` (finding 2).
        if crate::agent_ops::is_protected_ref(&branch) {
            let valid_exact_head = target_head_sha
                .as_deref()
                .is_some_and(crate::daemon::ci_watch::is_full_commit_sha);
            if !valid_exact_head {
                continue;
            }
        }
        // Same classifier + lazy PR-open as the GC (is_branch_open runs only if the
        // max-age threshold is crossed) → binding_state never drifts from cleanup.
        let (lifecycle, expiry_reason) =
            match crate::daemon::ci_watch::classify_subscribed_watch_expiry(&watch, now_utc, || {
                crate::daemon::pr_state::is_branch_open(home, &repo, &branch)
            }) {
                Some(r) => ("expired", Some(r.as_str())),
                None => ("polling", None),
            };
        // Binding identity is repo+branch (finding 1): a same-branch watch on a
        // DIFFERENT repo is NOT current.
        let current_binding =
            !current_repo.is_empty() && repo == current_repo && branch == current_branch;
        let detail = json!({
            "repo": &repo,
            "branch": &branch,
            "target_head_sha": &target_head_sha,
            "expires_at": &watch.expires_at,
            "current_binding": current_binding,
            "last_terminal_seen_at": &watch.last_terminal_seen_at,
            "lifecycle": lifecycle,
            "expiry_reason": expiry_reason,
        });
        rows.push((repo, branch, target_head_sha.unwrap_or_default(), detail));
    }
    rows.sort_by(|a, b| {
        (a.0.as_str(), a.1.as_str(), a.2.as_str()).cmp(&(b.0.as_str(), b.1.as_str(), b.2.as_str()))
    });
    rows.into_iter().map(|(_, _, _, d)| d).collect()
}
