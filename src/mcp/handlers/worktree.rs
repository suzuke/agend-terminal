//! MCP handlers for daemon-managed worktree lifecycle:
//! - `release_worktree` (Sprint 53 P0-X) — hard release of an agent's
//!   worktree + binding via `worktree_pool::release_full`.
//! - `gc_dry_run` (Sprint 53 P1-4) — operator-callable visibility into
//!   Phase 4 GC candidates without grepping app.log. Wraps the existing
//!   `worktree_pool::gc_dry_run`; non-destructive.
//!
//! Both tools are operator-callable + agent-callable.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `release_worktree`.
///
/// Required arg: `agent` (string).
///
/// Returns:
/// - `released`: `true` when the binding was cleared (worktree may still
///   exist if removal was skipped or partially failed — see `error`).
/// - `worktree_removed`: `true` when the worktree directory was actually
///   removed via `git worktree remove --force` (or fallback).
/// - `binding_removed`: `true` when `runtime/<agent>/binding.json` was
///   deleted.
/// - `error`: optional human-readable error. Idempotent second call returns
///   `released: false, error: "no binding for agent X"` per spec.
pub(crate) fn handle_release_worktree(
    home: &Path,
    args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    let agent = match args["agent"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'agent'"}),
    };
    if let Err(e) = crate::agent::validate_name(agent) {
        return json!({"error": e});
    }
    let outcome = crate::worktree_pool::release_full(home, agent);
    serde_json::to_value(&outcome).unwrap_or_else(|_| json!({"error": "serialize failed"}))
}

/// MCP tool: `gc_dry_run` (Sprint 53 P1-4).
///
/// Operator-callable surface for Phase 4 GC visibility. Wraps the existing
/// `worktree_pool::gc_dry_run` (which the daemon also runs hourly), enriches
/// each candidate with `branch` / `leased_at` / `released_at` parsed out of
/// its `.agend-managed` marker, and formats the result as either a
/// human-readable string list (default) or a JSON object.
///
/// Non-destructive: dry-run only. Phase 4 cutover (actual `git worktree remove`
/// of candidates) stays behind the separate `AGEND_WORKTREE_GC=1` switch.
///
/// Optional arg:
/// - `format`: `"human"` (default) or `"json"`. Anything else → graceful
///   error so a typo doesn't silently produce one of the formats.
pub(crate) fn handle_gc_dry_run(home: &Path, args: &Value, _sender: &Option<Sender>) -> Value {
    let format = args["format"].as_str().unwrap_or("human");
    if format != "human" && format != "json" {
        return json!({
            "error": format!("invalid 'format': {format:?} — expected 'human' or 'json'")
        });
    }

    let candidates = crate::worktree_pool::gc_dry_run(home);
    let enriched: Vec<Value> = candidates
        .iter()
        .map(|c| {
            let (branch, leased_at, released_at) = read_marker_fields(&c.path);
            json!({
                "agent": c.agent,
                "branch": branch,
                "path": c.path.display().to_string(),
                "leased_at": leased_at,
                "released_at": released_at,
                "reason": c.reason,
            })
        })
        .collect();

    if format == "json" {
        return json!({
            "candidates": enriched,
            "count": enriched.len(),
        });
    }

    // Human format. Empty list still emits a single-line summary so an
    // operator running the tool always gets visible feedback.
    let mut out = String::new();
    out.push_str(&format!(
        "Worktree GC dry-run candidates: {} found",
        enriched.len()
    ));
    if !enriched.is_empty() {
        out.push_str("\n\n");
        for (i, c) in enriched.iter().enumerate() {
            let agent = c["agent"].as_str().unwrap_or("?");
            let branch = c["branch"].as_str().unwrap_or("?");
            let leased = c["leased_at"].as_str().unwrap_or("?");
            let released = c["released_at"].as_str().unwrap_or("(none)");
            out.push_str(&format!(
                "  {n}. {agent} / {branch} — leased {leased}, released {released}\n",
                n = i + 1,
            ));
        }
    }

    json!({
        "format": "human",
        "count": enriched.len(),
        "text": out,
    })
}

/// Parse `branch=`, `leased_at=`, `released_at=` lines out of a worktree's
/// `.agend-managed` marker file. Missing fields → `None` (rendered as
/// `null` in JSON, `(none)` in human output). Failed read → all None.
fn read_marker_fields(wt_path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let marker = wt_path.join(".agend-managed");
    let Ok(content) = std::fs::read_to_string(&marker) else {
        return (None, None, None);
    };
    let mut branch = None;
    let mut leased = None;
    let mut released = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("branch=") {
            branch = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("leased_at=") {
            leased = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("released_at=") {
            released = Some(v.to_string());
        }
    }
    (branch, leased, released)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        let h = std::env::temp_dir().join(format!(
            "agend-p0x-handler-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    #[test]
    fn handler_rejects_missing_agent() {
        let home = tmp_home("no-agent");
        let result = handle_release_worktree(&home, &json!({}), &None);
        assert_eq!(
            result["error"].as_str(),
            Some("missing 'agent'"),
            "missing agent must surface clear error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_rejects_invalid_agent_name() {
        let home = tmp_home("bad-name");
        // Agent names with `..` are rejected by validate_name.
        let result = handle_release_worktree(&home, &json!({"agent": "../etc/passwd"}), &None);
        assert!(
            result.get("error").is_some(),
            "invalid agent name must error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_idempotent_no_binding_returns_released_false() {
        // Production-smoke: handler called via the same path the MCP layer
        // uses (`handle_release_worktree`). With no binding, must return
        // released:false and error indicating no binding — not panic.
        let home = tmp_home("idem-no-binding");
        let result = handle_release_worktree(&home, &json!({"agent": "ghost"}), &None);
        assert_eq!(
            result["released"].as_bool(),
            Some(false),
            "missing binding must report released=false: {result}"
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap_or("")
                .contains("no binding"),
            "error must indicate no binding: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 53 P1-4: gc_dry_run handler tests ────────────────────────
    //
    // These exercise the production handler. The fixture builders mimic
    // what `worktree_pool::lease` + `release_full` would produce on the
    // filesystem so the underlying `gc_candidates` scan + marker parser
    // see realistic state.
    //
    // Regression-proof: stub `worktree_pool::gc_dry_run` to return an empty
    // Vec → `gc_dry_run_production_smoke_lists_stale_lease` FAILS (count=0
    // when fixture has 1 candidate). Restore → PASS.

    /// Fixture: build a worktree dir with the `.agend-managed` marker shape
    /// `lease()` writes, plus a `released_at=` line older than the GC grace
    /// window (24h) so `gc_candidates` accepts it. No binding is written, so
    /// the candidate scan's "no active binding" gate passes.
    fn make_stale_candidate(home: &std::path::Path, agent: &str, branch: &str) {
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join(agent);
        std::fs::create_dir_all(&wt).unwrap();
        // 48h ago — comfortably past the 24h grace.
        let leased_at = (chrono::Utc::now() - chrono::Duration::hours(72)).to_rfc3339();
        let released_at = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(".agend-managed"),
            format!(
                "agent={agent}\nbranch={branch}\nleased_at={leased_at}\nreleased_at={released_at}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn gc_dry_run_no_candidates_returns_empty() {
        // Empty workspace → 0 candidates, both formats render the empty
        // shape without panicking.
        let home = tmp_home("gc-empty");
        let human = handle_gc_dry_run(&home, &json!({}), &None);
        assert_eq!(human["count"].as_u64(), Some(0), "empty count: {human}");
        assert_eq!(human["format"].as_str(), Some("human"));
        assert!(human["text"].as_str().unwrap_or("").contains("0 found"));

        let json_out = handle_gc_dry_run(&home, &json!({"format": "json"}), &None);
        assert_eq!(json_out["count"].as_u64(), Some(0));
        assert_eq!(
            json_out["candidates"].as_array().map(|a| a.len()),
            Some(0),
            "json candidates must be empty array: {json_out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_human_format_default() {
        // No `format` arg → human default. Output text must contain agent
        // + branch + the "found" summary.
        let home = tmp_home("gc-human");
        make_stale_candidate(&home, "agent-foo", "fix/branch-x");

        let result = handle_gc_dry_run(&home, &json!({}), &None);
        assert_eq!(result["format"].as_str(), Some("human"));
        assert_eq!(result["count"].as_u64(), Some(1));
        let text = result["text"].as_str().unwrap_or("");
        assert!(text.contains("1 found"), "summary: {text}");
        assert!(text.contains("agent-foo"), "must name agent: {text}");
        assert!(text.contains("fix/branch-x"), "must name branch: {text}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_json_format_explicit() {
        // `format=json` → structured candidates array with marker-derived
        // fields. Asserts the response is itself valid JSON-as-Value.
        let home = tmp_home("gc-json");
        make_stale_candidate(&home, "agent-bar", "feat/branch-y");

        let result = handle_gc_dry_run(&home, &json!({"format": "json"}), &None);
        assert_eq!(result["count"].as_u64(), Some(1));
        let arr = result["candidates"]
            .as_array()
            .expect("candidates must be an array");
        assert_eq!(arr.len(), 1);
        let c = &arr[0];
        assert_eq!(c["agent"], "agent-bar");
        assert_eq!(c["branch"], "feat/branch-y");
        assert!(
            c["leased_at"].as_str().is_some(),
            "leased_at must be present: {c}"
        );
        assert!(
            c["released_at"].as_str().is_some(),
            "released_at must be present (this fixture released the lease): {c}"
        );
        assert!(c["path"].as_str().is_some());
        assert!(c["reason"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_invalid_format_rejected() {
        // Typo in format must error gracefully so the operator notices
        // immediately rather than silently getting one of the formats.
        let home = tmp_home("gc-bad-format");
        let result = handle_gc_dry_run(&home, &json!({"format": "xml"}), &None);
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            err.contains("invalid 'format'") && err.contains("xml"),
            "expected invalid-format error mentioning 'xml': {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_production_smoke_lists_stale_lease() {
        // Production smoke: full path through `handle_gc_dry_run` →
        // `worktree_pool::gc_dry_run` → `gc_candidates` → marker scan.
        // Exercises the same code paths the MCP layer dispatches `gc_dry_run`
        // calls into.
        let home = tmp_home("gc-prod-smoke");
        make_stale_candidate(&home, "agent-prod", "feat/prod-stale");

        let result = handle_gc_dry_run(&home, &json!({"format": "json"}), &None);
        assert_eq!(
            result["count"].as_u64(),
            Some(1),
            "production smoke must surface the stale lease via the MCP path"
        );
        let candidates = result["candidates"].as_array().expect("candidates array");
        let agents: Vec<&str> = candidates
            .iter()
            .filter_map(|c| c["agent"].as_str())
            .collect();
        assert!(
            agents.contains(&"agent-prod"),
            "agent-prod must appear in candidates: {agents:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
