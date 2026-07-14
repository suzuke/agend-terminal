//! #2755 idempotent bind-reuse for `repo action=checkout`, extracted from `checkout.rs`
//! to keep that handler under the MCP-handler LOC ceiling (same split pattern as
//! `source_resolve.rs`). The full fail-closed reuse contract lives here: deadlock-safe
//! exact-path lock transfer, CAS re-read, canonical daemon-managed provenance, then
//! sync-to-final-HEAD → strict recursive init → exact gitlink verification.

use super::checkout::redact_paths;
use super::checkout_txn::PathLockGuard;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Reconcile + return an EXISTING bound worktree for THIS agent+branch (idempotent
/// `bind:true` re-checkout), or a structured fail-closed error. Consumes `path_lock`
/// (the DERIVED-path lock A) — it is dropped so the EXACT bound-path lock B can be taken
/// without an A→B inversion. Always returns a response (never falls through).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_reuse_bound_worktree(
    home: &Path,
    instance_name: &str,
    branch: &str,
    source_canonical: &Path,
    source_path: &str,
    wt: PathBuf,
    path_lock: PathLockGuard,
    auto_created_branch: bool,
    fetch_attempted: bool,
    expected_head: Option<&str>,
) -> Value {
    let wt_str = wt.display().to_string();
    tracing::info!(
        instance = instance_name,
        %branch,
        path = %wt_str,
        "repo checkout bind:true idempotent — agent already bound to this branch, revalidating + self-healing existing worktree"
    );
    // #2755 R3 (B4): the binding's worktree `wt` may be a DIFFERENT path than the DERIVED
    // `worktree_dir` the path-lock A guards (dispatch layout worktrees/<agent>/<branch> vs
    // the derived <agent>-<source>). Mutating `wt` under A is a lock-for-the-wrong-path
    // hole. Under the OUTER branch-lease (held by the caller), DROP A and acquire the
    // EXACT lock B for `wt` (no A→B inversion → no deadlock), then CAS re-read + validate
    // provenance BEFORE any destructive sync/reset/init.
    drop(path_lock);
    let wt_mangled = wt
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let wt_lock = match super::checkout_txn::acquire_path_lock(home, &wt, &wt_mangled) {
        Ok(g) => g,
        Err(e) => {
            return json!({
                "error": format!(
                    "reuse: could not acquire provisioning lock for the bound worktree: {}",
                    redact_paths(&e.to_string())
                ),
                "code": "reuse_path_lock",
                "branch": branch,
            })
        }
    };
    if !wt_lock.guards(&wt) {
        return json!({
            "error": "reuse: provisioning lock identity does not match the bound worktree path",
            "code": "reuse_path_lock_identity",
            "branch": branch,
        });
    }
    // CAS re-read + provenance from ONE fresh read under lock B. The binding must STILL
    // map this exact branch+worktree (a concurrent release/rebind may have changed it),
    // AND — fail closed (decision d-…38; signature verification is out of #2755 scope) —
    // the bound worktree must be a DAEMON-MANAGED worktree of the REQUESTED source.
    let reread = crate::binding::read(home, instance_name);
    let maps_exact = reread.as_ref().is_some_and(|r| {
        r.get("branch").and_then(|v| v.as_str()) == Some(branch)
            && r.get("worktree").and_then(|v| v.as_str()) == Some(wt_str.as_str())
    });
    if !maps_exact {
        return json!({
            "error": "reuse: binding changed while acquiring the worktree lock — retry",
            "code": "reuse_binding_race",
            "branch": branch,
        });
    }
    let bound_source_ok = reread
        .as_ref()
        .and_then(|r| r.get("source_repo").and_then(|v| v.as_str()))
        .and_then(|s| Path::new(s).canonicalize().ok())
        .map(|c| c.as_path() == source_canonical)
        .unwrap_or(false);
    // #2755 R4 (item 4): CANONICALIZE both the bound worktree and the pool — a symlink or
    // `..` path inside the pool can otherwise point at an EXTERNAL worktree yet pass a
    // lexical `starts_with`, and the sync/reset/init below would then mutate the resolved
    // external target. Require the CANONICAL worktree to be a strict descendant of the
    // CANONICAL pool (fail closed if either cannot be canonicalized) AND carry the marker.
    let managed = wt.join(crate::worktree_pool::MANAGED_MARKER).is_file()
        && match (wt.canonicalize(), home.join("worktrees").canonicalize()) {
            (Ok(cwt), Ok(cpool)) => cwt.starts_with(&cpool) && cwt != cpool,
            _ => false,
        };
    if !bound_source_ok || !managed {
        return json!({
            "error": "reuse refused: the bound worktree is not a daemon-managed worktree of the requested source at the exact bound path",
            "code": "reuse_provenance",
            "branch": branch,
        });
    }
    if let Some(expected) = expected_head {
        let actual_worktree =
            crate::git_helpers::git_cmd(&wt, &["rev-parse", "HEAD"]).unwrap_or_default();
        let actual_worktree = actual_worktree.trim().to_string();
        if !actual_worktree.eq_ignore_ascii_case(expected) {
            return json!({
                "error": format!(
                    "expected_head {expected} does not match bound worktree HEAD {actual_worktree}"
                ),
                "code": "expected_head_drift",
                "expected_head": expected,
                "actual_head": actual_worktree,
                "branch": branch,
            });
        }
        let actual_branch = crate::git_helpers::git_cmd(
            source_canonical,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .unwrap_or_default();
        let actual_branch = actual_branch.trim().to_string();
        if !actual_branch.eq_ignore_ascii_case(expected) {
            return json!({
                "error": format!(
                    "expected_head {expected} does not match branch HEAD {actual_branch}"
                ),
                "code": "expected_head_drift",
                "expected_head": expected,
                "actual_head": actual_branch,
                "branch": branch,
            });
        }
    }
    // #2755 R3 (B1): sync to the FINAL HEAD FIRST (an externally advanced branch may
    // change/add gitlinks), THEN strict recursive init, THEN verify EXACT gitlink commits
    // — any sync/init/verify failure returns NO success (fail closed), never a bound:true
    // over a broken tree.
    if expected_head.is_none() {
        if let Err(e) = crate::worktree::sync_worktree_to_head_strict(&wt) {
            return json!({
                "error": format!("reuse: sync to HEAD failed: {}", redact_paths(&e)),
                "code": "reuse_sync_failed",
                "branch": branch,
            });
        }
    }
    if let Err(e) = crate::worktree::init_submodules_strict(&wt) {
        return json!({
            "error": format!("reuse: recursive submodule init failed: {}", redact_paths(&e)),
            "code": "reuse_submodule_init_failed",
            "branch": branch,
        });
    }
    if let Err(e) = crate::worktree::verify_submodules_at_gitlinks(&wt) {
        return json!({
            "error": format!(
                "reuse: submodule gitlink verification failed: {}",
                redact_paths(&e)
            ),
            "code": "reuse_gitlink_mismatch",
            "branch": branch,
        });
    }
    if let Some(expected) = expected_head {
        let actual_worktree =
            crate::git_helpers::git_cmd(&wt, &["rev-parse", "HEAD"]).unwrap_or_default();
        let actual_worktree = actual_worktree.trim().to_string();
        let actual_branch = crate::git_helpers::git_cmd(
            source_canonical,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .unwrap_or_default();
        let actual_branch = actual_branch.trim().to_string();
        if !actual_worktree.eq_ignore_ascii_case(expected) {
            return json!({
                "error": format!(
                    "expected_head {expected} does not match bound worktree HEAD {actual_worktree}"
                ),
                "code": "expected_head_drift",
                "expected_head": expected,
                "actual_head": actual_worktree,
                "branch": branch,
            });
        }
        if !actual_branch.eq_ignore_ascii_case(expected) {
            return json!({
                "error": format!(
                    "expected_head {expected} does not match branch HEAD {actual_branch}"
                ),
                "code": "expected_head_drift",
                "expected_head": expected,
                "actual_head": actual_branch,
                "branch": branch,
            });
        }
    }
    json!({
        "path": wt_str,
        "source": source_path,
        "branch": branch,
        "bound": true,
        "idempotent": true,
        "auto_created_branch": auto_created_branch,
        "fetch_attempted": fetch_attempted,
    })
}
