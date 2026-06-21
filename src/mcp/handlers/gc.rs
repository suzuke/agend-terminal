//! MCP handler for `gc_dry_run` (Sprint 53 P1-4 Phase 4 visibility,
//! non-destructive — wraps `worktree_pool::gc_dry_run`). Split out of
//! `worktree.rs` (t-…50793-9 FIX5) to keep that file under the handler LOC cap
//! and to co-locate the new `target/` retention-sweep preview with the GC
//! visibility surface it extends.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `gc_dry_run` — Phase 4 GC visibility wrapping
/// `worktree_pool::gc_dry_run`. Non-destructive (removal gated behind
/// `AGEND_WORKTREE_GC=1`). Also previews the `target/` retention-sweep
/// candidates (t-…50793-9) so an operator sees what will be reclaimed before
/// the gc_tick runs it. Format: `"human"` (default) or `"json"`.
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

    // t-…50793-9: also preview the stale `target/` build dirs the retention
    // sweep would reclaim, so an operator sees what will be deleted before/while
    // the gc_tick runs it. Non-destructive (this only enumerates).
    let targets = crate::worktree_pool::target_sweep_dry_run(home);
    let target_total_bytes: u64 = targets.iter().map(|t| t.size_bytes).sum();
    let target_json: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "agent": t.agent,
                "target": t.target.display().to_string(),
                "idle_secs": t.idle_secs,
                "size_bytes": t.size_bytes,
            })
        })
        .collect();

    if format == "json" {
        return json!({
            "candidates": enriched,
            "count": enriched.len(),
            "target_sweep": target_json,
            "target_sweep_count": target_json.len(),
            "target_sweep_total_bytes": target_total_bytes,
            // VET condition (no-silent-coverage-cap): always surface the sweep's
            // scope boundary so the figures never imply the disk problem is solved.
            "target_sweep_scope": crate::worktree_pool::TARGET_SWEEP_SCOPE_NOTE,
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

    out.push_str(&format!(
        "\nStale target/ build dirs eligible for sweep: {} (~{} MB)",
        target_json.len(),
        target_total_bytes / (1024 * 1024)
    ));
    for (i, t) in targets.iter().enumerate() {
        out.push_str(&format!(
            "\n  {n}. {agent} — {path} (idle {h}h, ~{mb} MB)",
            n = i + 1,
            agent = t.agent,
            path = t.target.display(),
            h = t.idle_secs / 3600,
            mb = t.size_bytes / (1024 * 1024),
        ));
    }
    // VET condition: honest scope boundary on every preview.
    out.push_str(&format!(
        "\n({})",
        crate::worktree_pool::TARGET_SWEEP_SCOPE_NOTE
    ));

    json!({
        "format": "human",
        "count": enriched.len(),
        "target_sweep_count": target_json.len(),
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
            "agend-gc-handler-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&h).unwrap();
        h
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
