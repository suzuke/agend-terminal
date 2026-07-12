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
    // RED-STUB (finding-3 RED commit): the real projection lands in the GREEN
    // commit; here ci_watches_detail is empty so the projection matrix fails RED.
    let _ = (home, agent, current_repo, current_branch);
    Vec::new()
}
