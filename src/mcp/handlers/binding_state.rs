//! MCP handler for `binding_state` (Sprint 58 Wave 3 PR-2 #8) — daemon-
//! side bind-tracking diagnostic.
//!
//! Operator + agent introspection surface that reports:
//! - `runtime/<agent>/binding.json` content (branch, task_id, worktree,
//!   source_repo, issued_at)
//! - on-disk reality checks, three distinct signals (#1486):
//!   - `worktree_exists_on_disk` — the dir is literally present (true even for
//!     an empty shell whose contents were cleared).
//!   - `worktree_valid` — there's a USABLE git worktree (`.git` or the
//!     `.agend-managed` marker). **Use this for the rebuild decision** — an
//!     empty shell reads `exists_on_disk=true && valid=false`.
//!   - `marker_present` — the daemon's `.agend-managed` marker is present.
//!     **Use this for ownership / GC** (is this a daemon-managed worktree the
//!     release path may remove, vs an operator-owned one R14 leaves alone).
//! - CI watch subscriptions (`ci-watches/*.json` enumeration)
//! - in-memory bind-in-flight guard state
//!   (Sprint 55 P0-B EC11 — exposed via
//!   `dispatch_hook::is_bind_in_flight`)
//! - cross-branch holders (other agents whose binding references the
//!   same branch — P0-1.5 invariant violation surface)
//!
//! Pairs with the comprehensive `release_worktree` cleanup landed in
//! the same PR (#9): `release_full` defensively clears the in-memory
//! bind-in-flight set so a panic between `BindGuard::try_acquire` and
//! the implicit `Drop` doesn't silently block re-bind.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `binding_state` (Sprint 58 Wave 3 PR-2 #8).
///
/// Operator + agent introspection surface for daemon-side bind tracking.
/// Reports the structured state of an agent's binding so lease-block
/// recovery debugging doesn't require grepping
/// `runtime/<agent>/binding.json`, cross-referencing `git worktree list`,
/// and checking `ci-watches/*.json` separately. Single tool call returns
/// everything at once.
///
/// Required arg: `instance` (string).
///
/// Returns (bound case):
/// ```json
/// {
///   "agent": "dev",
///   "bound": true,
///   "branch": "...",
///   "task_id": "...",
///   "worktree": "...",
///   "source_repo": "...",
///   "issued_at": "...",
///   "worktree_exists_on_disk": true,
///   "worktree_valid": true,
///   "marker_present": true,
///   "signature_valid": true,
///   "ci_watches": ["repo:branch", ...],
///   "bind_in_flight": false,
///   "cross_branch_holders": []
/// }
/// ```
///
/// `signature_valid` is diagnostic only (on-disk body+sig via shim-same verify).
///
/// Returns (unbound case):
/// ```json
/// {
///   "agent": "dev",
///   "bound": false,
///   "bind_in_flight": false,
///   "ci_watches": [],
///   "cross_branch_holders": []
/// }
/// ```
///
/// `bind_in_flight` exposes the dispatch-hook in-memory guard
/// (Sprint 55 P0-B EC11) so operators can detect a concurrent bind
/// race without the MCP layer's logs. `cross_branch_holders` lists
/// any other agents whose binding currently references the SAME branch
/// (Sprint 57 lease-block recovery surface — when this is non-empty
/// AND the queried agent has no binding, the operator can immediately
/// see who's holding the branch).
fn probe_target_identity(wt_path: &Path, expected_branch: &str, expected_worktree: &str) -> Value {
    let actual_head = crate::git_helpers::git_cmd(wt_path, &["rev-parse", "HEAD"]);
    let actual_branch = crate::git_helpers::git_cmd(wt_path, &["symbolic-ref", "--short", "HEAD"]);
    match (&actual_head, &actual_branch) {
        (Ok(head), Ok(branch)) => {
            let matches = branch == expected_branch;
            json!({
                "expected_branch": expected_branch,
                "expected_worktree": expected_worktree,
                "actual_branch": branch,
                "actual_head": head,
                "probe_status": "ok",
                "matches_binding": matches,
            })
        }
        _ => {
            let err = actual_head
                .as_ref()
                .err()
                .map(|e| format!("{e}"))
                .or_else(|| actual_branch.as_ref().err().map(|e| format!("{e}")))
                .unwrap_or_else(|| "unknown probe failure".into());
            json!({
                "expected_branch": expected_branch,
                "expected_worktree": expected_worktree,
                "probe_status": "error",
                "probe_error": err,
                "matches_binding": false,
            })
        }
    }
}

pub(crate) fn handle_binding_state(home: &Path, args: &Value, _sender: &Option<Sender>) -> Value {
    let agent = match args["instance"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(agent);

    let binding = crate::binding::read(home, agent);
    let bind_in_flight = crate::mcp::handlers::dispatch_hook::is_bind_in_flight(home, agent);
    let ci_watches = enumerate_ci_watches_for_agent(home, agent);
    // PR2 L3 visibility: surface pending dispatch metadata alongside
    // binding state so operators investigating a stuck binding can see
    // in one tool call whether the agent owes a reply or is waiting
    // for one. Empty arrays for agents with no pending sidecars
    // (cross-team-safe: non-fixup teams without explicit thresholds
    // never record sidecars, so this is a no-op for them).
    let (dispatched_waiting_for, pending_response_to) =
        crate::daemon::dispatch_idle::pending_for_instance(home, agent);

    if let Some(b) = binding {
        // Bound: enrich with on-disk reality checks so the operator
        // can spot half-state (binding present, worktree gone — or
        // worktree present but missing the .agend-managed marker).
        let wt_str = b["worktree"].as_str().unwrap_or("");
        let wt_path = Path::new(wt_str);
        let worktree_exists_on_disk = !wt_str.is_empty() && wt_path.exists();
        let marker_present = worktree_exists_on_disk && wt_path.join(".agend-managed").exists();
        // #1486: `worktree_exists_on_disk` is literal (the dir is present) and
        // returns true even for an empty shell left behind when the worktree's
        // contents were cleared (external cleanup / CC-compaction restart). That
        // misleads an agent into thinking the worktree is usable and skipping a
        // rebuild. `worktree_valid` answers the question that actually matters
        // for rebuild — "is there a USABLE git worktree here?" — by requiring a
        // `.git` (any git worktree) or the daemon's `.agend-managed` marker.
        // (Additive: the literal field is unchanged, so the empty-shell half-
        // state is now visibly `exists_on_disk=true && valid=false`.)
        let worktree_valid = worktree_exists_on_disk
            && (wt_path.join(".git").exists() || wt_path.join(".agend-managed").exists());

        // #t-…83936-4: `worktree_valid` above checks that the worktree's `.git`
        // EXISTS, but a linked worktree's `.git` is a POINTER file
        // (`gitdir: <canonical>/.git/worktrees/<name>`) — it survives even after
        // the canonical it points to is deleted, so `.git`-exists is falsely
        // "valid". `worktree_resolves` does the REAL liveness check (canonical
        // present + is a git repo, AND the worktree's gitdir actually resolves).
        // ADDITIVE (lead Q1): `worktree_valid` semantics are untouched so existing
        // rebuild-decision consumers don't shift; callers that need strictness read
        // `worktree_resolves` / `invalid_reason`. All paths are ABSOLUTE (from
        // binding.json), so `metadata`/`git -C` resolve by path, never cwd — this
        // is what lets it catch the case where the daemon's own cwd is the deleted
        // canonical (the 40-min-silent incident). Same class as the dev3
        // dangling-.git hazard (t-…83936-2), folded in here.
        let source_repo = b["source_repo"].as_str().unwrap_or("");
        let canonical_present = !source_repo.is_empty()
            && std::fs::metadata(source_repo).is_ok()
            && crate::git_helpers::git_ok(Path::new(source_repo), &["rev-parse", "--git-dir"]);
        let gitdir_resolves = worktree_exists_on_disk
            && crate::git_helpers::git_ok(wt_path, &["rev-parse", "--git-dir"]);
        let worktree_resolves = canonical_present && gitdir_resolves;
        let invalid_reason: Option<&str> = if !worktree_exists_on_disk {
            Some("worktree_missing")
        } else if !canonical_present {
            Some("canonical_missing")
        } else if !gitdir_resolves {
            Some("gitdir_dangling")
        } else {
            None
        };

        // Cross-branch holders: any other agent currently bound to
        // the same branch (should be 0 — `dispatch_auto_bind_lease`'s
        // P0-1.5 cross-agent uniqueness check enforces this — but
        // surfacing it makes a violation immediately visible).
        let branch = b["branch"].as_str().unwrap_or("");
        let cross_branch_holders = cross_branch_holders_for(home, branch, agent);

        // PR2 F2: diagnostic HMAC status (shim-parity; see binding::signature_valid).
        let signature_valid = crate::binding::signature_valid(home, agent);

        // S2 finding-1: the agent's CURRENT binding identity is repo+branch. Derive
        // the owner/repo slug from the binding source_repo so `current_binding` in
        // ci_watches_detail matches BOTH repo and branch (a same-branch watch on a
        // different repo is NOT current). Non-derivable remote ⇒ "" ⇒ nothing current.
        // r2 (#2746): provider-neutral so a reachable Bitbucket-Cloud watch's own
        // binding matches (a GitHub-only derivation regressed it to ""); MUST stay in
        // lockstep with watch-storage canonicalization — see provider_neutral_slug.
        let current_repo = crate::mcp::handlers::dispatch_hook::derive_repo_slug_any_forge_pub(
            Path::new(source_repo),
        )
        .unwrap_or_default();

        let target_identity = probe_target_identity(wt_path, branch, wt_str);

        json!({
            "agent": agent,
            "bound": true,
            "branch": branch,
            "task_id": b["task_id"].as_str().unwrap_or(""),
            "worktree": wt_str,
            "source_repo": b["source_repo"].as_str().unwrap_or(""),
            "issued_at": b["issued_at"].as_str().unwrap_or(""),
            "worktree_exists_on_disk": worktree_exists_on_disk,
            "worktree_valid": worktree_valid,
            "worktree_resolves": worktree_resolves,
            "invalid_reason": invalid_reason,
            "marker_present": marker_present,
            "signature_valid": signature_valid,
            "ci_watches": ci_watches,
            "ci_watches_detail": enumerate_ci_watches_detail_for_agent(home, agent, &current_repo, branch),
            "bind_in_flight": bind_in_flight,
            "cross_branch_holders": cross_branch_holders,
            "dispatched_waiting_for": dispatched_waiting_for,
            "pending_response_to": pending_response_to,
            "target_identity": target_identity,
        })
    } else {
        json!({
            "agent": agent,
            "bound": false,
            "bind_in_flight": bind_in_flight,
            "ci_watches": ci_watches,
            "ci_watches_detail": enumerate_ci_watches_detail_for_agent(home, agent, "", ""),
            "cross_branch_holders": Vec::<String>::new(),
            "dispatched_waiting_for": dispatched_waiting_for,
            "pending_response_to": pending_response_to,
        })
    }
}

// S2: the ci-watch enumerators (byte-for-byte `ci_watches` strings + the additive
// `ci_watches_detail` projection) live in a sibling module so binding_state.rs
// stays under the MCP-handler LOC ceiling (file_size_invariant — the same reason
// the tests are `#[path]` siblings).
#[path = "binding_state_ci_watches.rs"]
mod ci_watches;
use ci_watches::{enumerate_ci_watches_detail_for_agent, enumerate_ci_watches_for_agent};

/// Return list of agent names (other than `exclude_agent`) whose
/// binding currently references `branch`. P0-1.5 enforces uniqueness
/// at bind time — this enumerator surfaces any violation so it's
/// immediately visible via `binding_state`.
fn cross_branch_holders_for(home: &Path, branch: &str, exclude_agent: &str) -> Vec<String> {
    if branch.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (other, v) in crate::binding::binding_scan_all(home) {
        if other == exclude_agent {
            continue;
        }
        if v["branch"].as_str() == Some(branch) {
            out.push(other);
        }
    }
    out.sort();
    out
}

// #t-…83936-4 protection ① liveness tests (worktree_resolves / invalid_reason)
// live in a sibling file loaded via `#[path]` so binding_state.rs stays under
// the MCP-handler LOC ceiling (file_size_invariant skips "*test*"-named files) —
// the Sprint 54/55 file_size_invariant pattern also used by channel.rs.
#[cfg(test)]
#[path = "binding_state_liveness_tests.rs"]
mod liveness_tests;

// PR2 architecture F2: signature_valid observability pins (sibling file).
#[cfg(test)]
#[path = "binding_state_signature_tests.rs"]
mod signature_tests;

// S2 additive ci_watches_detail projection matrix (sibling file — LOC-exempt).
#[cfg(test)]
#[path = "binding_state_detail_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod detail_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mcp::handlers::worktree::handle_release_worktree;

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        let h = std::env::temp_dir().join(format!(
            "agend-binding-state-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    fn write_binding(home: &std::path::Path, agent: &str, branch: &str, worktree: &str) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = json!({
            "version": 1,
            "agent": agent,
            "task_id": "test-task",
            "branch": branch,
            "worktree": worktree,
            "source_repo": "/tmp/source-repo",
            "issued_at": "2026-05-09T00:00:00Z",
        });
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn binding_state_returns_current_binding_for_bound_agent() {
        let home = tmp_home("bound");
        // Set up a worktree dir with .agend-managed marker so the
        // diagnostic reports `marker_present: true`.
        let wt = home.join("worktree-dir");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "agent=alpha\nbranch=feature/x\n").unwrap();
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(
            result["bound"].as_bool(),
            Some(true),
            "must report bound: {result}"
        );
        assert_eq!(result["agent"].as_str(), Some("alpha"));
        assert_eq!(result["branch"].as_str(), Some("feature/x"));
        assert_eq!(result["task_id"].as_str(), Some("test-task"));
        assert_eq!(result["source_repo"].as_str(), Some("/tmp/source-repo"));
        assert_eq!(
            result["worktree_exists_on_disk"].as_bool(),
            Some(true),
            "worktree dir exists, must report true: {result}"
        );
        assert_eq!(
            result["worktree_valid"].as_bool(),
            Some(true),
            ".agend-managed marker → valid worktree: {result}"
        );
        assert_eq!(
            result["marker_present"].as_bool(),
            Some(true),
            ".agend-managed marker present, must report true: {result}"
        );
        assert_eq!(result["bind_in_flight"].as_bool(), Some(false));
        assert_eq!(result["cross_branch_holders"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_returns_unbound_for_agent_with_no_binding() {
        let home = tmp_home("unbound");
        let result = handle_binding_state(&home, &json!({"instance": "ghost"}), &None);
        assert_eq!(
            result["bound"].as_bool(),
            Some(false),
            "no binding → bound:false: {result}"
        );
        assert_eq!(result["agent"].as_str(), Some("ghost"));
        assert_eq!(result["bind_in_flight"].as_bool(), Some(false));
        assert!(
            result["ci_watches"].as_array().unwrap().is_empty(),
            "no watches: {result}"
        );
        assert!(
            result["cross_branch_holders"]
                .as_array()
                .unwrap()
                .is_empty(),
            "no holders: {result}"
        );
        // Bound case fields should NOT be present in unbound shape.
        assert!(result["branch"].is_null(), "no branch field when unbound");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_rejects_missing_agent() {
        let home = tmp_home("no-agent");
        let result = handle_binding_state(&home, &json!({}), &None);
        assert_eq!(
            result["error"].as_str(),
            Some("missing 'instance'"),
            "missing instance error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_rejects_invalid_agent_name() {
        let home = tmp_home("bad-name");
        let result = handle_binding_state(&home, &json!({"instance": "../etc/passwd"}), &None);
        assert!(
            result.get("error").is_some(),
            "invalid name error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_reports_worktree_missing_when_dir_deleted() {
        // Half-state detection: binding.json says worktree at /X but
        // /X has been deleted (e.g. operator manually `rm -rf`'d).
        // The diagnostic must surface this so the operator knows to
        // run release_worktree to clean up the orphan binding.
        let home = tmp_home("half");
        let wt = home.join("never-existed");
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(result["bound"].as_bool(), Some(true));
        assert_eq!(
            result["worktree_exists_on_disk"].as_bool(),
            Some(false),
            "worktree was never created → false: {result}"
        );
        assert_eq!(
            result["worktree_valid"].as_bool(),
            Some(false),
            "no dir → not a valid worktree: {result}"
        );
        assert_eq!(
            result["marker_present"].as_bool(),
            Some(false),
            "no worktree → no marker: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_reports_marker_missing_for_operator_owned_worktree() {
        // Half-state detection: worktree exists on disk but lacks
        // .agend-managed marker — operator created it manually, daemon
        // shouldn't ever delete it. The diagnostic exposes this so
        // operators can disambiguate "daemon-managed orphan that
        // release_worktree can clean" from "operator-owned worktree
        // that R14 safety leaves alone".
        let home = tmp_home("no-marker");
        let wt = home.join("operator-worktree");
        std::fs::create_dir_all(&wt).unwrap();
        // #1486: a REAL operator-owned worktree has a `.git` (git worktree add
        // always creates one) but NO `.agend-managed` marker. The fixture must
        // carry `.git` to faithfully represent that — an empty dir is not an
        // operator worktree, it's a half-state shell (covered separately by
        // `binding_state_reports_invalid_for_empty_shell_dir`).
        std::fs::write(wt.join(".git"), "gitdir: /somewhere/.git/worktrees/x\n").unwrap();
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(
            result["worktree_exists_on_disk"].as_bool(),
            Some(true),
            "dir exists: {result}"
        );
        assert_eq!(
            result["worktree_valid"].as_bool(),
            Some(true),
            "operator worktree has .git → valid: {result}"
        );
        assert_eq!(
            result["marker_present"].as_bool(),
            Some(false),
            "no .agend-managed marker → false: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_reports_invalid_for_empty_shell_dir() {
        // #1486 RED: the dir is present but has NEITHER `.git` NOR the
        // `.agend-managed` marker — an empty shell left behind when the
        // worktree's contents were cleared (external cleanup / CC-compaction
        // restart). Pre-#1486 `worktree_exists_on_disk` reported true and there
        // was no `worktree_valid`, so an agent saw a "present" worktree and
        // skipped the rebuild it actually needed. Now: exists_on_disk stays
        // true (literal) but worktree_valid is false (the rebuild signal).
        let home = tmp_home("empty-shell");
        let wt = home.join("empty-shell");
        std::fs::create_dir_all(&wt).unwrap();
        // No `.git`, no `.agend-managed`.
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(
            result["worktree_exists_on_disk"].as_bool(),
            Some(true),
            "the shell dir is literally present: {result}"
        );
        assert_eq!(
            result["worktree_valid"].as_bool(),
            Some(false),
            "empty shell (no .git, no marker) is NOT a usable worktree: {result}"
        );
        assert_eq!(
            result["marker_present"].as_bool(),
            Some(false),
            "no .agend-managed marker: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_reports_valid_for_daemon_managed_worktree() {
        // #1486: a daemon-managed worktree (`.agend-managed` marker, no `.git`
        // in this minimal fixture) is valid via the marker arm of the
        // `.git || .agend-managed` check — and marker_present is also true.
        let home = tmp_home("managed-valid");
        let wt = home.join("managed-worktree");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "agent=alpha\n").unwrap();
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(result["worktree_valid"].as_bool(), Some(true), "{result}");
        assert_eq!(result["marker_present"].as_bool(), Some(true), "{result}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_surfaces_cross_branch_holders() {
        // Two agents bound to the same branch (P0-1.5 invariant
        // violation — should never happen via dispatch_auto_bind_lease,
        // but if a stale binding leaks the diagnostic must reveal it).
        let home = tmp_home("cross");
        write_binding(&home, "alpha", "shared-branch", "/tmp/wt-alpha");
        write_binding(&home, "beta", "shared-branch", "/tmp/wt-beta");
        write_binding(&home, "gamma", "different-branch", "/tmp/wt-gamma");

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        let holders = result["cross_branch_holders"].as_array().unwrap();
        let names: Vec<&str> = holders.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            names.contains(&"beta"),
            "beta also holds shared-branch, must surface: {names:?}"
        );
        assert!(
            !names.contains(&"alpha"),
            "alpha is the queried agent, must NOT self-list: {names:?}"
        );
        assert!(
            !names.contains(&"gamma"),
            "gamma holds a different branch, must NOT appear: {names:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2550 W3 Wave1 pin: a corrupt sibling binding.json must not abort the
    /// scan — a later agent's valid, matching binding must still surface.
    #[test]
    fn cross_branch_holders_for_skips_corrupt_binding_continues_scan_2550_w3() {
        let home = tmp_home("cross-corrupt");
        let corrupt_dir = crate::paths::runtime_dir(&home).join("corrupt-agent");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join("binding.json"), b"not valid json").unwrap();
        write_binding(&home, "beta", "shared-branch", "/tmp/wt-beta");

        let holders = cross_branch_holders_for(&home, "shared-branch", "alpha");
        assert!(
            holders.contains(&"beta".to_string()),
            "a corrupt sibling binding must not block finding beta: {holders:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_handles_concurrent_bind_self_calls_correctly() {
        // The bind_in_flight flag exposes the dispatch-hook in-memory
        // guard. BindGuard is private, so we can't directly seed an
        // entry — but we can pin that the unbound, never-bound case
        // reports `bind_in_flight: false` (the trivial baseline; the
        // RAII guard is exercised in dispatch_hook integration tests).
        let home = tmp_home("inflight");
        let result = handle_binding_state(&home, &json!({"instance": "concurrent"}), &None);
        assert_eq!(
            result["bind_in_flight"].as_bool(),
            Some(false),
            "no in-flight bind seeded, must report false: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn release_worktree_clears_bind_in_flight_defensive_helper() {
        // Sprint 58 Wave 3 PR-2 #9: comprehensive cleanup. Invoke the
        // defensive `clear_bind_in_flight` directly and confirm it's
        // a no-op on a clean state (no panic, no error). The integration
        // — that release_full calls it after binding::unbind — is
        // exercised in the worktree_pool integration tests; here we
        // pin that the helper itself is safe to call repeatedly.
        let home = tmp_home("clear-inflight");
        crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(&home, "ghost");
        crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(&home, "ghost");
        // No assertion needed beyond "doesn't panic" — the function
        // returns nothing; the warn-log fires only when an entry was
        // actually removed.
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn release_worktree_idempotent_on_unbound_agent() {
        // #1465: release on an agent with no binding is an idempotent
        // SUCCESS no-op — released:true, already_released:true, no error
        // (was released:false + "no binding" pre-#1465). Also pins that the
        // defensive clear_bind_in_flight call doesn't panic on a clean state.
        let home = tmp_home("release-idem");
        let r1 = handle_release_worktree(&home, &json!({"instance": "never-bound"}), &None);
        assert_eq!(r1["released"].as_bool(), Some(true), "{r1}");
        assert_eq!(r1["already_released"].as_bool(), Some(true), "{r1}");
        let r2 = handle_release_worktree(&home, &json!({"instance": "never-bound"}), &None);
        assert_eq!(
            r2["released"].as_bool(),
            Some(true),
            "second call same idempotent shape: {r2}"
        );
        assert_eq!(r2["already_released"].as_bool(), Some(true), "{r2}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_after_release_reports_unbound_clean_state() {
        // Regression-proof against the Sprint 57 lease-block surface:
        // after release_worktree, binding_state must report bound:false,
        // bind_in_flight:false, no cross_branch_holders, and no leaked
        // ci_watches. If any layer leaks state, this assertion fails.
        let home = tmp_home("post-release");
        let wt = home.join("wt-x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "agent=alpha\n").unwrap();
        write_binding(&home, "alpha", "feature/x", wt.to_str().unwrap());

        // Pre-release: bound.
        let pre = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(pre["bound"].as_bool(), Some(true));

        // Release.
        let _ = handle_release_worktree(&home, &json!({"instance": "alpha"}), &None);

        // Post-release: unbound, clean.
        let post = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(
            post["bound"].as_bool(),
            Some(false),
            "post-release must report unbound: {post}"
        );
        assert_eq!(
            post["bind_in_flight"].as_bool(),
            Some(false),
            "in-flight guard cleared post-release: {post}"
        );
        assert!(
            post["ci_watches"].as_array().unwrap().is_empty(),
            "no leaked watches: {post}"
        );
        assert!(
            post["cross_branch_holders"].as_array().unwrap().is_empty(),
            "no cross-branch holders: {post}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn binding_state_lists_ci_watches_for_bound_agent() {
        // Defensive bonus: if the agent is subscribed to a CI watch,
        // binding_state must surface it. This pairs with
        // unsubscribe_all_ci_watches_for_agent (release_full's
        // existing cleanup) — operators can verify pre/post-release
        // that watches were removed.
        let home = tmp_home("watches");
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        let watch = json!({
            "repo": "owner/repo",
            "branch": "feature/x",
            "subscribers": [
                {"instance": "alpha"},
                {"instance": "beta"},
            ],
            "instance": "alpha",
        });
        std::fs::write(
            ci_dir.join("watch1.json"),
            serde_json::to_string_pretty(&watch).unwrap(),
        )
        .unwrap();
        write_binding(&home, "alpha", "feature/x", "/tmp/wt");

        let result = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        let watches = result["ci_watches"].as_array().unwrap();
        let entries: Vec<&str> = watches.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            entries.contains(&"owner/repo:feature/x"),
            "alpha's watch must surface as owner/repo:feature/x: {entries:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── PR2 L3 visibility: binding_state surfaces dispatch metadata ──

    /// Both bound and unbound binding_state responses must surface the
    /// new `dispatched_waiting_for` + `pending_response_to` fields read
    /// from `<home>/pending-dispatches/*.json`. Operator investigating
    /// a stuck binding can see in one call whether the agent owes a
    /// reply or is waiting for one.
    #[test]
    fn binding_state_surfaces_pending_dispatch_metadata() {
        let home = tmp_home("l3-binding");
        // Seed a sidecar where "alpha" is the dispatcher waiting on
        // "beta", and "alpha" is also the target of an inbound
        // dispatch from "gamma" (covers both arrays in one shot).
        crate::daemon::dispatch_idle::record_dispatch(
            &home,
            "alpha",
            "beta",
            Some("t-out"),
            "task",
            600,
        )
        .unwrap();
        crate::daemon::dispatch_idle::record_dispatch(
            &home,
            "gamma",
            "alpha",
            Some("t-in"),
            "task",
            600,
        )
        .unwrap();
        // Unbound alpha — exercises the unbound JSON shape.
        let unbound = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(unbound["bound"].as_bool(), Some(false));
        let dw = unbound["dispatched_waiting_for"]
            .as_array()
            .expect("dispatched_waiting_for must be an array (unbound case)");
        assert_eq!(dw.len(), 1);
        assert_eq!(dw[0]["target"].as_str(), Some("beta"));
        assert_eq!(dw[0]["correlation_id"].as_str(), Some("t-out"));
        let pr = unbound["pending_response_to"]
            .as_array()
            .expect("pending_response_to must be an array (unbound case)");
        assert_eq!(pr.len(), 1);
        assert_eq!(pr[0]["dispatcher"].as_str(), Some("gamma"));
        assert_eq!(pr[0]["correlation_id"].as_str(), Some("t-in"));
        // Bound alpha — exercises the bound JSON shape carries the
        // same L3 fields.
        let wt = home.join("worktree-l3");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "agent=alpha\n").unwrap();
        write_binding(&home, "alpha", "feature/l3", wt.to_str().unwrap());
        let bound = handle_binding_state(&home, &json!({"instance": "alpha"}), &None);
        assert_eq!(bound["bound"].as_bool(), Some(true));
        assert_eq!(
            bound["dispatched_waiting_for"].as_array().map(|a| a.len()),
            Some(1),
            "bound shape must include dispatched_waiting_for"
        );
        assert_eq!(
            bound["pending_response_to"].as_array().map(|a| a.len()),
            Some(1),
            "bound shape must include pending_response_to"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
