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

/// MCP tool: `bind_self` (Sprint 54 P1-7). Lets any instance proactively
/// bind itself to a worktree without going through the dispatch hook.
/// Required args: `repo` / `source_repo` (one of), `branch`. Returns
/// `{bound, worktree_path, branch}` on success or `{error, code}` on
/// failure. Thin shim over `dispatch_auto_bind_lease` — bug fixes in
/// the dispatch path inherit automatically.
pub(crate) fn handle_bind_self(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let agent = match sender.as_ref().map(Sender::as_str) {
        Some(a) if !a.is_empty() => a,
        _ => {
            return json!({
                "error": "bind_self requires AGEND_INSTANCE_NAME — anonymous callers cannot bind",
                "code": "needs_identity"
            })
        }
    };
    let branch = match args["branch"].as_str() {
        Some(b) if !b.is_empty() => b,
        _ => return json!({"error": "missing 'branch'", "code": "missing_arg"}),
    };
    if !crate::agent_ops::validate_branch(branch) {
        return json!({
            "error": format!("invalid branch name '{branch}'"),
            "code": "invalid_branch"
        });
    }

    // Sprint 55 P0-B EC9: dual-arg shape with two-sprint deprecation cycle.
    // - `source_repo: <local path>` (NEW unified shape; daemon derives owner/repo)
    // - `repo: "owner/name"` (legacy GitHub slug; warn-log; removed Sprint 57)
    // Both present → reject as `ambiguous_args`. Neither → fleet.yaml fallback chain.
    let source_repo_arg = args["source_repo"].as_str().filter(|s| !s.is_empty());
    let repo_arg = args["repo"].as_str().filter(|s| !s.is_empty());
    if source_repo_arg.is_some() && repo_arg.is_some() {
        return json!({
            "error": "both 'source_repo' and 'repo' provided — pass exactly one",
            "code": "ambiguous_args"
        });
    }
    if repo_arg.is_some() {
        tracing::warn!(
            %agent,
            "bind_self(repo=...) is deprecated; use bind_self(source_repo=<local-path>) — Sprint 55 warning, Sprint 57 removal"
        );
    }
    let source_repo_path = source_repo_arg.map(std::path::PathBuf::from);

    // Issue #689: reject path traversal in source_repo
    if let Some(ref p) = source_repo_path {
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return json!({
                "error": "source_repo must not contain '..' (path traversal rejected)",
                "code": "path_traversal"
            });
        }
    }

    if args["rebase_mode"].as_bool().unwrap_or(false) {
        if let Err(e) = crate::mcp::handlers::force_release::rebase_clean_self(home, agent, branch)
        {
            return json!({"error": e, "code": "path_outside_pool"});
        }
    }

    // task_id="self" — clear distinction from real task IDs in the binding.json
    // audit trail. The tasks store doesn't need a row for a self-bind; the
    // string is purely a marker.
    match crate::mcp::handlers::dispatch_hook::dispatch_auto_bind_lease_with_source(
        home,
        agent,
        "self",
        branch,
        repo_arg,
        source_repo_path.as_deref(),
    ) {
        Ok(_outcome) => {
            // Successful bind: read back the worktree path from the binding
            // file we just wrote so the response reflects authoritative
            // state. #781 `DispatchOutcome` fields are dropped here —
            // surfacing them is a `bind_self` consumer follow-up.
            let binding_path = crate::paths::runtime_dir(home)
                .join(agent)
                .join("binding.json");
            let worktree_path = std::fs::read_to_string(&binding_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v["worktree"].as_str().map(String::from))
                .unwrap_or_default();
            json!({
                "bound": true,
                "worktree_path": worktree_path,
                "branch": branch,
            })
        }
        Err(err) => {
            // Map `DispatchError` to the pre-#781 string-code shape so
            // existing callers keep parsing the same surface. The follow-up
            // to dispatch on `err.code` belongs with bind_self's consumer
            // migration.
            let msg = err.message;
            let code = if msg.contains("E4.5") {
                "e4_5_protected_branch"
            } else if msg.contains("already leased") {
                "cross_agent_conflict"
            } else {
                "lease_failed"
            };
            json!({"error": msg, "code": code})
        }
    }
}

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
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let outcome = crate::worktree_pool::release_full(home, agent, dry_run);
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

    // ── Sprint 54 P1-7: bind_self handler tests ─────────────────────────
    //
    // These exercise `handle_bind_self` directly — same path the MCP layer
    // uses. The helper sets up a real git repo + fleet.yaml entry so
    // `worktree_pool::lease` can actually create the worktree (matches
    // dispatch_hook/tests.rs setup_test_repo).
    //
    // Regression-proof anchor: replace the body of
    // `dispatch_auto_bind_lease` with `Ok(())` (skip the actual bind) →
    // `bind_self_creates_binding_and_worktree` fails because binding.json
    // never gets written. PR description carries the captured FAIL
    // signature.

    fn p17_setup_repo(home: &std::path::Path, agent: &str) -> std::path::PathBuf {
        let repo = crate::paths::workspace_dir(home).join(agent);
        std::fs::create_dir_all(&repo).ok();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        // #781 Phase 3 r1 (Path A): populate `refs/remotes/origin/main`
        // for strict `ensure_branch_exists`; file:/// URL so
        // derive_repo returns None.
        let git = |a: &[&str]| -> Option<std::process::Output> {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&repo)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .ok()
        };
        git(&["remote", "add", "origin", "file:///dev/null/agend-fix"]);
        if let Some(o) = git(&["rev-parse", "HEAD"]).filter(|o| o.status.success()) {
            let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
            git(&["update-ref", "refs/remotes/origin/main", &sha]);
        }
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            format!(
                "instances:\n  {agent}:\n    backend: claude\n    working_directory: {}\n",
                repo.display()
            ),
        )
        .ok();
        repo
    }

    fn sender_for(name: &str) -> Option<crate::identity::Sender> {
        crate::identity::Sender::new(name)
    }

    #[test]
    fn bind_self_creates_binding_and_worktree() {
        // Gate 1: a successful bind_self produces binding.json + worktree
        // dir + .agend-managed marker. Mirrors the dispatch hook's
        // happy path because we go through the same helper.
        //
        // EMPIRICAL REGRESSION-PROOF ANCHOR: replacing
        // `dispatch_auto_bind_lease` body with `Ok(())` makes this test
        // fail with "binding.json must exist after bind_self".
        let home = std::env::temp_dir().join(format!("agend-p17-self-{}-ok", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        p17_setup_repo(&home, "agent-self");

        let resp = handle_bind_self(
            &home,
            &json!({"repo": "owner/name", "branch": "feat/p17"}),
            &sender_for("agent-self"),
        );
        assert_eq!(
            resp["bound"].as_bool(),
            Some(true),
            "bind_self must succeed: {resp}"
        );
        let worktree_path = resp["worktree_path"]
            .as_str()
            .expect("worktree_path in success response");
        assert!(!worktree_path.is_empty(), "worktree_path must be populated");

        let binding_path = crate::paths::runtime_dir(&home)
            .join("agent-self")
            .join("binding.json");
        assert!(
            binding_path.exists(),
            "binding.json must exist after bind_self"
        );
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&binding_path).expect("read binding"))
                .expect("parse binding");
        assert_eq!(v["branch"].as_str(), Some("feat/p17"));
        assert_eq!(
            v["task_id"].as_str(),
            Some("self"),
            "self-bind must record task_id=self"
        );

        // Worktree dir + .agend-managed marker per P0-X / P1-7.
        let wt = std::path::Path::new(worktree_path);
        assert!(wt.exists(), "worktree dir must exist: {worktree_path}");
        assert!(
            wt.join(".agend-managed").exists(),
            ".agend-managed marker must exist: {worktree_path}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_idempotent_same_agent_same_branch() {
        // Gate 2: a second bind_self call from the same agent on the
        // same branch is idempotent. The first lease creates the
        // worktree; the second sees the existing daemon-managed
        // worktree on the matching branch and succeeds without
        // mutating state.
        let home = std::env::temp_dir().join(format!("agend-p17-self-{}-idem", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        p17_setup_repo(&home, "agent-idem");

        let args = json!({"repo": "owner/name", "branch": "feat/idem"});
        let r1 = handle_bind_self(&home, &args, &sender_for("agent-idem"));
        assert_eq!(r1["bound"].as_bool(), Some(true), "first bind: {r1}");
        let r2 = handle_bind_self(&home, &args, &sender_for("agent-idem"));
        assert_eq!(
            r2["bound"].as_bool(),
            Some(true),
            "second bind on same branch must be idempotent: {r2}"
        );
        assert_eq!(
            r1["worktree_path"], r2["worktree_path"],
            "worktree path must be stable across idempotent calls"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_rejects_main_branch_with_e4_5() {
        // Gate 3: protected-branch invariant. Calling bind_self on
        // 'main' returns the E4.5 rejection from worktree_pool::lease,
        // mapped to a stable code so agents can branch on it.
        let home = std::env::temp_dir().join(format!("agend-p17-self-{}-e45", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        p17_setup_repo(&home, "agent-e45");

        let resp = handle_bind_self(
            &home,
            &json!({"repo": "owner/name", "branch": "main"}),
            &sender_for("agent-e45"),
        );
        assert!(
            resp.get("error").is_some(),
            "main branch must error: {resp}"
        );
        assert_eq!(
            resp["code"].as_str(),
            Some("e4_5_protected_branch"),
            "error code must surface E4.5 class: {resp}"
        );

        // No side-effects on rejection.
        let binding = crate::paths::runtime_dir(&home)
            .join("agent-e45")
            .join("binding.json");
        assert!(
            !binding.exists(),
            "rejected bind_self must not write binding.json"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_rejects_cross_agent_branch_conflict() {
        // Gate 4: P0-1.5 cross-agent registry — agent A binds, agent B
        // attempts the same branch → B is rejected.
        let home =
            std::env::temp_dir().join(format!("agend-p17-self-{}-cross", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        p17_setup_repo(&home, "agent-A");
        p17_setup_repo(&home, "agent-B");

        let r1 = handle_bind_self(
            &home,
            &json!({"repo": "owner/name", "branch": "feat/cross"}),
            &sender_for("agent-A"),
        );
        assert_eq!(r1["bound"].as_bool(), Some(true), "A binds first: {r1}");

        let r2 = handle_bind_self(
            &home,
            &json!({"repo": "owner/name", "branch": "feat/cross"}),
            &sender_for("agent-B"),
        );
        assert!(
            r2.get("error").is_some(),
            "B must be rejected on shared branch: {r2}"
        );
        assert_eq!(
            r2["code"].as_str(),
            Some("cross_agent_conflict"),
            "code must be cross_agent_conflict: {r2}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_then_release_worktree_clean_state() {
        // Gate 5: lifecycle round-trip. bind_self creates state;
        // release_worktree clears it. binding.json + worktree dir +
        // .agend-managed marker all gone after release.
        let home =
            std::env::temp_dir().join(format!("agend-p17-self-{}-cycle", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        p17_setup_repo(&home, "agent-cycle");

        let resp = handle_bind_self(
            &home,
            &json!({"repo": "owner/name", "branch": "feat/cycle"}),
            &sender_for("agent-cycle"),
        );
        assert_eq!(resp["bound"].as_bool(), Some(true));
        let worktree_path = resp["worktree_path"]
            .as_str()
            .expect("worktree path")
            .to_string();
        let binding = home
            .join("runtime")
            .join("agent-cycle")
            .join("binding.json");
        assert!(binding.exists());

        let release = handle_release_worktree(&home, &json!({"agent": "agent-cycle"}), &None);
        assert_eq!(
            release["released"].as_bool(),
            Some(true),
            "release must succeed: {release}"
        );

        assert!(!binding.exists(), "binding.json must be gone after release");
        assert!(
            !std::path::Path::new(&worktree_path).exists(),
            "worktree dir must be gone after release: {worktree_path}"
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod path_traversal_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_repo_with_dotdot_rejected() {
        let home = std::env::temp_dir().join("pt-test-1");
        std::fs::create_dir_all(&home).ok();
        let args = json!({"branch": "feat-x", "source_repo": "/tmp/../etc/passwd"});
        let sender = Some(crate::identity::Sender::new("agent-1").unwrap());
        let result = handle_bind_self(&home, &args, &sender);
        assert_eq!(result["code"].as_str(), Some("path_traversal"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn nested_traversal_rejected() {
        let home = std::env::temp_dir().join("pt-test-2");
        std::fs::create_dir_all(&home).ok();
        let args = json!({"branch": "feat-x", "source_repo": "/home/user/foo/../../etc"});
        let sender = Some(crate::identity::Sender::new("agent-2").unwrap());
        let result = handle_bind_self(&home, &args, &sender);
        assert_eq!(result["code"].as_str(), Some("path_traversal"));
        std::fs::remove_dir_all(&home).ok();
    }
}
