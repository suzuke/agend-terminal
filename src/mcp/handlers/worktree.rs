//! MCP handlers for daemon-managed worktree lifecycle. Operator- and
//! agent-callable: `bind_self` (Sprint 54 P1-7), `release_worktree`
//! (Sprint 53 P0-X), `gc_dry_run` (Sprint 53 P1-4 Phase 4 visibility,
//! non-destructive — wraps `worktree_pool::gc_dry_run`).

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `bind_self` (Sprint 54 P1-7). Lets any instance proactively
/// bind itself to a worktree on the named branch via the daemon's
/// standard lease lifecycle.
///
/// **When to use vs `repo action=checkout bind:true`** (#779 Option 1):
/// - **`bind:true`** — preferred for fresh-task dispatches where the
///   caller already knows the source repo (passes explicit
///   `repository_path` arg). Single-step atomic provision + bind.
/// - **`bind_self`** — preferred for mid-lifecycle scenarios:
///   (a) re-binding a recovered worktree via `rebase_mode=true`,
///   (b) binding via fleet.yaml-resolved source repo (caller has no
///   explicit `repository_path` arg available),
///   (c) post-`release_worktree` re-claim of the same branch.
///
/// Both paths share `dispatch_auto_bind_lease` so binding.json +
/// .agend-managed marker + auto watch_ci all land. Bug fixes in the
/// dispatch path inherit automatically.
///
/// Required args: `repository_path` / `repository` (one of), `branch`.
/// Returns `{bound, worktree_path, branch}` on success or `{error, code}`
/// on failure.
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
    // - `repository_path: <local path>` (NEW unified shape; daemon derives owner/repo)
    // - `repository: "owner/name"` (legacy GitHub slug; warn-log; removed Sprint 57)
    // Both present → reject as `ambiguous_args`. Neither → fleet.yaml fallback chain.
    let source_repo_arg = args["repository_path"].as_str().filter(|s| !s.is_empty());
    let repo_arg = args["repository"].as_str().filter(|s| !s.is_empty());
    if source_repo_arg.is_some() && repo_arg.is_some() {
        return json!({
            "error": "both 'repository_path' and 'repository' provided — pass exactly one",
            "code": "ambiguous_args"
        });
    }
    if repo_arg.is_some() {
        tracing::warn!(
            %agent,
            "bind_self(repository=...) is deprecated; use bind_self(repository_path=<local-path>) — Sprint 55 warning, Sprint 57 removal"
        );
    }
    let source_repo_path = source_repo_arg.map(std::path::PathBuf::from);

    // Issue #689: reject path traversal in repository_path
    if let Some(ref p) = source_repo_path {
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return json!({
                "error": "repository_path must not contain '..' (path traversal rejected)",
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

    let task_id = args["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    match crate::mcp::handlers::dispatch_hook::dispatch_auto_bind_lease_with_source(
        home,
        agent,
        task_id,
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
            // Map `DispatchError` to the pre-#781 string-code response shape, but
            // dispatch on the TYPED `err.code` — NOT message substrings
            // (smells#2 Pattern-A / de2eb8 finding #1). The old
            // `msg.contains("already leased")` misclassified the two
            // `LeaseConflict` producers whose message lacks that phrase
            // (lock-acquire failure, `worktree::create` None) as `lease_failed`
            // instead of `cross_agent_conflict`; matching the variant fixes that
            // and consolidates all lease conflicts under one stable code.
            use super::dispatch_hook::ErrorCode;
            let code = match err.code {
                ErrorCode::ProtectedBranch => "e4_5_protected_branch",
                ErrorCode::LeaseConflict => "cross_agent_conflict",
                _ => "lease_failed",
            };
            json!({"error": err.message, "code": code})
        }
    }
}

/// MCP tool: `release_worktree`. Required arg: `instance`. Returns
/// `{released, worktree_removed, binding_removed, error}` —
/// `released:true` clears binding; worktree removal via
/// `git worktree remove --force` (or fallback). Idempotent (#1465) — a
/// second call (no binding) returns `released:true, already_released:true`
/// (success no-op, no error).
pub(crate) fn handle_release_worktree(
    home: &Path,
    args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    let agent = match args["instance"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(agent);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    // #789: clean empty init commits before removal (best-effort).
    if !dry_run {
        if let Some(wt) = crate::binding::read(home, agent)
            .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
        {
            let _ = crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&wt).ok();
        }
    }
    let outcome = crate::worktree_pool::release_full(home, agent, dry_run);
    serde_json::to_value(&outcome).unwrap_or_else(|_| json!({"error": "serialize failed"}))
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
            Some("missing 'instance'"),
            "missing instance must surface clear error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_rejects_invalid_agent_name() {
        let home = tmp_home("bad-name");
        // Agent names with `..` are rejected by validate_name.
        let result = handle_release_worktree(&home, &json!({"instance": "../etc/passwd"}), &None);
        assert!(
            result.get("error").is_some(),
            "invalid agent name must error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_idempotent_no_binding_returns_success_noop() {
        // #1465: no binding → idempotent SUCCESS no-op (released:true,
        // already_released:true, no error; was released:false pre-#1465).
        let home = tmp_home("idem-no-binding");
        let result = handle_release_worktree(&home, &json!({"instance": "ghost"}), &None);
        assert_eq!(result["released"].as_bool(), Some(true), "{result}");
        assert_eq!(result["already_released"].as_bool(), Some(true), "{result}");
        assert!(
            result.get("error").is_none(),
            "no-op must not error: {result}"
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
            &json!({"repository": "owner/name", "branch": "feat/p17"}),
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
            Some(""),
            "self-bind without task_id arg must record empty task_id"
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

        let args = json!({"repository": "owner/name", "branch": "feat/idem"});
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
            &json!({"repository": "owner/name", "branch": "main"}),
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
        // attempts the same (source_repo, branch) → B is rejected.
        // #2117 P3b: the lease key is (source_repo, branch). Both agents claim the
        // SAME repo (via `repository_path`) and branch — the same-repo conflict P3b
        // preserves. (Cross-repo independence is covered by the dispatch-side
        // `cross_repo_same_branch_independent_p3b` test.)
        let home =
            std::env::temp_dir().join(format!("agend-p17-self-{}-cross", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let shared = p17_setup_repo(&home, "shared-repo");
        let shared_path = shared.display().to_string();

        let r1 = handle_bind_self(
            &home,
            &json!({"repository_path": shared_path, "branch": "feat/cross"}),
            &sender_for("agent-A"),
        );
        assert_eq!(r1["bound"].as_bool(), Some(true), "A binds first: {r1}");

        let r2 = handle_bind_self(
            &home,
            &json!({"repository_path": shared_path, "branch": "feat/cross"}),
            &sender_for("agent-B"),
        );
        assert!(
            r2.get("error").is_some(),
            "B must be rejected on the same (repo, branch): {r2}"
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
            &json!({"repository": "owner/name", "branch": "feat/cycle"}),
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

        let release = handle_release_worktree(&home, &json!({"instance": "agent-cycle"}), &None);
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
        let args = json!({"branch": "feat-x", "repository_path": "/tmp/../etc/passwd"});
        let sender = Some(crate::identity::Sender::new("agent-1").unwrap());
        let result = handle_bind_self(&home, &args, &sender);
        assert_eq!(result["code"].as_str(), Some("path_traversal"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn nested_traversal_rejected() {
        let home = std::env::temp_dir().join("pt-test-2");
        std::fs::create_dir_all(&home).ok();
        let args = json!({"branch": "feat-x", "repository_path": "/home/user/foo/../../etc"});
        let sender = Some(crate::identity::Sender::new("agent-2").unwrap());
        let result = handle_bind_self(&home, &args, &sender);
        assert_eq!(result["code"].as_str(), Some("path_traversal"));
        std::fs::remove_dir_all(&home).ok();
    }
}
