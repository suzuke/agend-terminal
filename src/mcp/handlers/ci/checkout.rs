use crate::agent_ops::validate_branch;
use crate::git_helpers::git_bypass;
use serde_json::{json, Value};
use std::path::Path;
// #2755 R3: response-mapping + marker-durability helpers live in a sibling module to
// keep this handler under the LOC ceiling (call sites below are unchanged).
use super::checkout_helpers::{rollback_response, sync_marker_contents, validate_expected_head};

use super::checkout_disposable::CheckoutPurpose;
pub(crate) use super::checkout_helpers::checkout_source;
pub(super) use super::checkout_helpers::redact_paths;

pub(crate) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let result = handle_checkout_repo_inner(home, args, instance_name);
    super::checkout_helpers::log_checkout_outcome(home, args, instance_name, &result);
    result
}

fn handle_checkout_repo_inner(home: &Path, args: &Value, instance_name: &str) -> Value {
    let source = match checkout_source(args) {
        Some(s) => s,
        None => return json!({"error": "missing 'repository_path'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    if !validate_branch(branch) {
        return json!({"error": format!("invalid branch name '{branch}'")});
    }
    // #778: bind:true atomically claims the provisioned worktree; false keeps inspection-only behavior.
    let bind = args["bind"].as_bool().unwrap_or(false);
    if bind && instance_name.is_empty() {
        return json!({
            "error": "bind=true requires AGEND_INSTANCE_NAME — anonymous callers cannot claim a worktree",
            "code": "needs_identity"
        });
    }
    let checkout_purpose = match super::checkout_disposable::parse(args, bind) {
        Ok(purpose) => purpose,
        Err(error) => return error,
    };
    // The bind transaction owns the per-agent lifecycle authority before any
    // provisioning preflight. Keep this permit through branch locking,
    // bind_full, commit, and exact rollback so checkout cannot race release or
    // rebase at the release→bind gap.
    let lifecycle_permit = if bind {
        match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
            home,
            instance_name,
            crate::mcp::handlers::dispatch_hook::LifecycleOperation::Bind,
        ) {
            Ok(permit) => Some(permit),
            Err(error) => {
                return json!({
                    "error": format!("checkout bind refused: {error}"),
                    "code": "lifecycle_conflict",
                });
            }
        }
    } else {
        None
    };
    if bind {
        if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
            return e;
        }
    }
    // Windows-safe path mangling collapses separators and drive-letter colons.
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace(['/', '\\', ':'], "_").replace('~', "")
    ));
    // #2158: source resolution is fail-closed and isolated in `source_resolve`.
    let (source_path, source_canonical) =
        match super::source_resolve::resolve_checkout_source_path(home, source) {
            Ok(pair) => pair,
            Err(e) => return e,
        };
    if let Some(e) = validate_expected_head(args, &source_path, branch) {
        return e;
    }
    if checkout_purpose == Some(CheckoutPurpose::DisposableReview) {
        if let Err(error) =
            super::checkout_disposable::preflight_branch(Path::new(&source_path), branch)
        {
            return error;
        }
    }
    let expected_ref = super::checkout_helpers::expected_creation_ref(args, &source_path, branch);
    std::fs::create_dir_all(worktree_dir.parent().unwrap_or(home)).ok();
    // #780: auto-create branch from `from_ref` when bind:true + branch
    // missing locally. #781 Piece 6 extracts the decision tree into
    // `dispatch_hook::ensure_branch_exists` so the same logic services
    // both this MCP-tool entry and the `send kind=task` dispatch hook
    // (single source of truth, no #780-vs-#781 logic drift). `bind:false`
    // preserves current back-compat (no auto-create) per decision
    // `d-20260514102305998399-0` scope.
    let mut auto_created_branch = false;
    let mut fetch_attempted = false;
    if bind {
        let src = Path::new(&source_path);
        // #2703: when the caller omits `from_ref`, default to the repo's DEFAULT
        // branch (origin/HEAD via `default_branch`), not a hard-coded origin/main —
        // mirrors the dispatch-path fix (dispatch_hook/mod.rs). An explicit
        // `from_ref` override is unchanged. Main-default repos: default_branch →
        // "main" → "origin/main", byte-identical to the prior literal.
        let default_base = format!("origin/{}", crate::git_helpers::default_branch(src));
        let from_ref = args["from_ref"].as_str().unwrap_or(&default_base);
        let creation_ref = expected_ref.as_deref().unwrap_or(from_ref);
        match crate::mcp::handlers::dispatch_hook::ensure_branch_exists(
            home,
            src,
            branch,
            creation_ref,
            instance_name,
        ) {
            Ok((created, fetched)) => {
                auto_created_branch = created;
                fetch_attempted = fetched;
                if checkout_purpose == Some(CheckoutPurpose::DisposableReview) && !created {
                    return json!({
                        "error": format!("disposable review branch '{branch}' was not created by this checkout"),
                        "code": "disposable_review_requires_new_branch",
                        "auto_created_branch": false,
                        "branch": branch,
                    });
                }
            }
            Err(err) => {
                let mut e = json!({
                    "error": err.message,
                    "code": serde_json::to_value(err.code).unwrap_or(json!("unknown")),
                    "stage": serde_json::to_value(err.stage).unwrap_or(json!("unknown")),
                    "fetch_attempted": err.fetch_attempted,
                });
                if let Some(raw) = err.raw {
                    e["raw"] = json!(raw);
                }
                return e;
            }
        }
    }
    // #1494: idempotent bind. If THIS agent already holds a binding on the SAME
    // branch with a live worktree (provisioned by the dispatch pre-build hook or a
    // prior `repo checkout`), the `git worktree add` below would fail "is already
    // checked out" (leased at a DIFFERENT dir than this handler's `<agent>-<source>`
    // scheme). Return the EXISTING worktree as success (#1465 idempotent-release
    // spirit). Cross-agent-safe: `binding::read` is per-agent, so a DIFFERENT agent
    // (or same-agent DIFFERENT branch) does NOT short-circuit — the genuine `git
    // worktree add` conflict error below is preserved.
    // #1882 (reviewer-2): repo checkout is the THIRD bind path (besides dispatch +
    // bind_self via dispatch_auto_bind_lease); hold the per-branch lease flock
    // across its check-then-act (cross-agent scan + idempotent read + worktree add +
    // bind_full) so a concurrent dispatch/checkout can't double-bind. Bind-only (a
    // `--detach` checkout writes no binding); guard lives to fn end (covers bind_full).
    // #2117 P3b: lease key is (source_repo, branch); `source_canonical` is the same
    // repo path bind_full persists below, so lock/scan/bind keys agree.
    let source_repo_str = source_canonical.display().to_string();
    let _lease_lock = if bind {
        match crate::binding::acquire_branch_lease_lock(home, &source_repo_str, branch) {
            Ok(g) => Some(g),
            Err(e) => {
                return json!({
                    "error": format!("could not acquire branch lease lock for '{branch}': {e}"),
                    "code": "lease_lock",
                    "branch": branch,
                })
            }
        }
    } else {
        None
    };
    // #2755 INNER provisioning lock — acquired BEFORE the idempotent-reuse check
    // and any provision so reuse AND fresh provision are both serialized on the
    // worktree PATH (a reuse must not bypass the lock or the recursive submodule
    // init). Declared AFTER `_lease_lock` ⇒ drops (releases) INNER-first. Journal +
    // lock live OUTSIDE the worktree so a rollback `remove --force` can't delete
    // the recovery record.
    let worktree_path_str = worktree_dir.display().to_string();
    let mangled = worktree_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let path_lock = match super::checkout_txn::acquire_path_lock(home, &worktree_dir, &mangled) {
        Ok(g) => g,
        Err(e) => {
            return json!({
                "error": format!(
                    "could not acquire provisioning lock for '{branch}': {}",
                    redact_paths(&e.to_string())
                ),
                "code": "path_lock",
                "branch": branch,
            })
        }
    };
    // Revalidate the held lock maps to the EXACT target path before any side effect
    // (fail-closed; the authority is the guard's normalized path).
    if !path_lock.guards(&worktree_dir) {
        return json!({
            "error": "provisioning lock identity does not match the target worktree path",
            "code": "path_lock_identity",
            "branch": branch,
        });
    }
    let txn_now = chrono::Utc::now();
    // Replay a journal left by a CRASHED prior provision of this path (removes a
    // stale worktree so a fresh add — or the reuse check below — sees clean state).
    if let Err(e) = super::checkout_txn::recover_stale(
        home,
        &mangled,
        &worktree_dir,
        &source_canonical.display().to_string(),
        txn_now,
        || {
            crate::git_helpers::git_bypass(
                Path::new(&source_path),
                &["worktree", "remove", "--force", &worktree_path_str],
            )
            .map(|o| o.status.success())
            .unwrap_or(false)
        },
    ) {
        return json!({"error": redact_paths(&e), "code": "stale_txn_rollback", "branch": branch});
    }
    if bind {
        // #1882: cross-agent P0-1.5 reject UNDER the lock — another agent holding
        // this branch is refused (mirrors the dispatch path's scan), rather than
        // leaning on `git worktree add`'s "already checked out" error. The
        // same-agent idempotent short-circuit below handles THIS agent re-checkout.
        if let Some(other) = crate::binding::scan_existing_branch_binding(
            home,
            &source_repo_str,
            branch,
            instance_name,
        ) {
            return json!({
                "error": format!(
                    "branch '{branch}' already leased by '{other}' — release first or use a different branch"
                ),
                "code": "cross_agent_conflict",
                "branch": branch,
            });
        }
        if let Some(existing) = crate::binding::read(home, instance_name) {
            let same_branch = existing.get("branch").and_then(|v| v.as_str()) == Some(branch);
            let live_wt = existing
                .get("worktree")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .filter(|p| p.exists());
            if same_branch {
                if let Some(wt) = live_wt {
                    // #2755: the full fail-closed reuse contract (deadlock-safe exact-path
                    // lock transfer, CAS re-read, canonical daemon-managed provenance, then
                    // sync-to-final-HEAD → strict init → gitlink verify) lives in the sibling
                    // `checkout_reuse` module to keep this handler under the LOC ceiling.
                    let mut reuse_resp = super::checkout_reuse::try_reuse_bound_worktree(
                        home,
                        instance_name,
                        branch,
                        &source_canonical,
                        &source_path,
                        wt,
                        path_lock,
                        auto_created_branch,
                        fetch_attempted,
                        args["expected_head"].as_str(),
                    );
                    // #6: echo expected_head/actual_head on idempotent reuse —
                    // re-read the actual HEAD from the worktree rather than
                    // echoing the expected value (the worktree may have diverged).
                    if let Some(expected) = args["expected_head"].as_str() {
                        if reuse_resp.get("error").is_some() {
                            return reuse_resp;
                        }
                        let wt_path = reuse_resp["path"].as_str().unwrap_or(&worktree_path_str);
                        let actual =
                            crate::git_helpers::git_cmd(Path::new(wt_path), &["rev-parse", "HEAD"])
                                .unwrap_or_default();
                        let actual = actual.trim();
                        reuse_resp["actual_head"] = json!(actual);
                        reuse_resp["expected_head"] = json!(expected);
                    }
                    return reuse_resp;
                }
                let existing_task_id = existing
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !existing_task_id.is_empty() {
                    return json!({
                        "error": format!(
                            "stale binding for branch '{branch}' points at a missing worktree - release first before checkout"
                        ),
                        "code": "stale_binding",
                        "branch": branch,
                    });
                }
            }
        }
    }
    // (INNER path-lock, identity revalidation, and stale-txn recovery were done
    // ABOVE — before the reuse check. `bind:true` omits `--detach` below so HEAD
    // lands on the named branch, #778.)
    // Bounded worktree rollback (LOCAL git via bypass), reused by every failure
    // path below so each checked-save failure leaves no orphan.
    let remove_worktree = || {
        crate::git_helpers::git_bypass(
            Path::new(&source_path),
            &["worktree", "remove", "--force", &worktree_path_str],
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
    };
    // Prepared: durably journal the intent BEFORE any filesystem side effect. A
    // failed save here is fatal-but-clean (no side effect yet).
    let mut journal = super::checkout_txn::Journal::prepared(
        super::checkout_txn::new_nonce(),
        worktree_path_str.clone(),
        source_canonical.display().to_string(),
        branch,
        bind,
        txn_now.to_rfc3339(),
    );
    if journal.save(home, &mangled).is_err() {
        return json!({
            "error": "could not persist checkout transaction journal",
            "code": "journal_write",
            "stage": "prepared",
            "branch": branch,
        });
    }
    let git_args: Vec<&str> = if bind {
        vec!["worktree", "add", &worktree_path_str, branch]
    } else {
        vec!["worktree", "add", "--detach", &worktree_path_str, branch]
    };
    match git_bypass(Path::new(&source_path), &git_args) {
        Ok(o) if o.status.success() => {
            // WorktreeAdded — CHECKED save: the worktree now EXISTS, so a save
            // failure must roll it back or the durable record under-reports on-disk
            // state (a crash would then orphan it).
            journal.advance(super::checkout_txn::Phase::WorktreeAdded);
            if journal.save(home, &mangled).is_err() {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    "could not persist WorktreeAdded journal",
                    "journal_write",
                    "worktree_added",
                    branch,
                );
            }
            let mut resp =
                json!({"path": worktree_path_str, "source": source_path, "branch": branch});
            // #1275 + #2755: write `.agend-managed` FAIL-CLOSED — a missing marker
            // breaks release_worktree/GC cleanup, so a write failure rolls back
            // rather than returning a half-managed worktree.
            // arch14 (d-20260719234211852352-4): canonical four-field identity —
            // source_repo= included so the deep-validated path-addressed release
            // accepts checkout-provisioned worktrees.
            let marker_path = worktree_dir.join(crate::worktree_pool::MANAGED_MARKER);
            if std::fs::write(
                &marker_path,
                format!(
                    "agent={instance_name}\nbranch={branch}\nsource_repo={source_path}\nleased_at={}\n",
                    chrono::Utc::now().to_rfc3339()
                ),
            )
            .is_err()
            {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    "marker write failed",
                    "marker_failed",
                    "marker_durable",
                    branch,
                );
            }
            // #2755 R3 (independent P1.4): make the marker CONTENTS durable BEFORE the
            // parent-dir fsync and the MarkerDurable phase advance. A sync failure rolls
            // back fail-closed — never record MarkerDurable (or later Committed success)
            // over a non-durable marker.
            if let Err(e) = sync_marker_contents(&marker_path) {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    &format!("marker fsync failed: {}", redact_paths(&e.to_string())),
                    "marker_fsync_failed",
                    "marker_durable",
                    branch,
                );
            }
            // #2755 R4 (item 5): OBSERVE the parent-dir (dirent) durability on Unix — a
            // failure must NOT advance MarkerDurable over a non-durable directory entry.
            if let Err(e) = crate::store::fsync_parent_dir_checked(&marker_path) {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    &format!(
                        "marker dirent fsync failed: {}",
                        redact_paths(&e.to_string())
                    ),
                    "marker_fsync_failed",
                    "marker_durable",
                    branch,
                );
            }
            journal.advance(super::checkout_txn::Phase::MarkerDurable);
            if journal.save(home, &mangled).is_err() {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    "could not persist MarkerDurable journal",
                    "journal_write",
                    "marker_durable",
                    branch,
                );
            }
            // #2755 SubmodulesReady phase: `git worktree add` materializes the
            // superproject (`.gitmodules` + gitlinks) but leaves submodule dirs
            // EMPTY, so a build with path-dependency submodules (e.g.
            // vendor/agentic-git) fails on a freshly provisioned worktree.
            // Recursively init them; a failure ABORTS the transaction — roll the
            // worktree back (arm retained intent + `remove --force`; a mere prune
            // can't remove a still-present dir) and return a structured error,
            // never a half-provisioned tree. Runs for bind AND non-bind (a
            // reviewer/triage inspection worktree needs its submodule content too).
            if let Err(e) = crate::worktree::init_submodules_strict(&worktree_dir) {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                let mut err = rollback_response(
                    outcome,
                    &format!("submodule init failed: {}", redact_paths(&e)),
                    "submodule_init_failed",
                    "submodules_ready",
                    branch,
                );
                if bind {
                    err["fetch_attempted"] = json!(fetch_attempted);
                    err["auto_created_branch"] = json!(auto_created_branch);
                }
                return err;
            }
            // #2755 R4 (item 2): a successful `submodule update --init --recursive` is NOT
            // proof the tree is buildable — `submodule.<name>.update=none` makes it exit 0
            // while leaving the submodule uninitialized (`-` in status). Verify the EXACT
            // gitlink commits BEFORE advancing SubmodulesReady; a mismatch rolls back
            // fail-closed (mirrors the reuse path's verify).
            if let Err(e) = crate::worktree::verify_submodules_at_gitlinks(&worktree_dir) {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                let mut err = rollback_response(
                    outcome,
                    &format!(
                        "submodule gitlink verification failed after init: {}",
                        redact_paths(&e)
                    ),
                    "submodule_gitlink_mismatch",
                    "submodules_ready",
                    branch,
                );
                if bind {
                    err["fetch_attempted"] = json!(fetch_attempted);
                    err["auto_created_branch"] = json!(auto_created_branch);
                }
                return err;
            }
            journal.advance(super::checkout_txn::Phase::SubmodulesReady);
            if journal.save(home, &mangled).is_err() {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {},
                );
                return rollback_response(
                    outcome,
                    "could not persist SubmodulesReady journal",
                    "journal_write",
                    "submodules_ready",
                    branch,
                );
            }
            if let Some(expected) = args["expected_head"].as_str() {
                if let Some(err) = super::checkout_helpers::rollback_if_expected_head_drift(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    Path::new(&source_path),
                    branch,
                    expected,
                    Path::new(&worktree_path_str),
                    true,
                    auto_created_branch,
                    "submodules_ready",
                ) {
                    return err;
                }
            }
            let mut bound_fingerprint = None;
            if bind {
                // #2533: optional task_id attributes self-claim to a task (in-dispatch).
                let task_id = args["task_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("");
                let provenance =
                    checkout_purpose.map(|purpose| {
                        purpose.provenance(args["expected_head"].as_str().expect(
                            "disposable_review validates expected_head before provisioning",
                        ))
                    });
                if let Err(e) = crate::binding::bind_full_with_provenance(
                    home,
                    instance_name,
                    task_id,
                    branch,
                    &worktree_dir,
                    &source_canonical,
                    true, // #2158 GR1: agent self-claim (repo checkout bind:true) → notify operator
                    provenance,
                ) {
                    // #1310: rollback worktree on binding failure to prevent orphans
                    tracing::warn!(
                        %branch, path = %worktree_dir.display(),
                        error = %e,
                        "bind_full failed after worktree add — rolling back worktree"
                    );
                    // #1899 + #2755: retained-intent rollback (arm journal + bounded
                    // `remove --force`; bind_full failed ⇒ no binding to unbind).
                    let outcome = super::checkout_txn::rollback_failed(
                        home,
                        &mangled,
                        &mut journal,
                        txn_now,
                        remove_worktree,
                        || {},
                    );
                    if matches!(outcome, super::checkout_txn::RollbackOutcome::Removed)
                        && auto_created_branch
                    {
                        super::checkout_disposable::rollback_auto_created_branch(
                            Path::new(&source_path),
                            branch,
                            args["expected_head"].as_str().unwrap_or(""),
                        );
                    }
                    return rollback_response(
                        outcome,
                        &format!("bind_full failed: {}", redact_paths(&e.to_string())),
                        "bind_rollback",
                        "bind_full",
                        branch,
                    );
                }
                if checkout_purpose.is_none() {
                    crate::binding::try_augment_review_lease(
                        home,
                        instance_name,
                        task_id,
                        branch,
                        &source_canonical,
                    );
                }
                bound_fingerprint =
                    match crate::binding::snapshot_guarded_binding(home, instance_name) {
                        Ok(crate::binding::GuardedBinding::Known { fingerprint, .. }) => {
                            Some(fingerprint)
                        }
                        other => {
                            // Binding bytes exist but their exact destructive identity
                            // cannot be proven. Arm retained rollback intent and leave
                            // both binding + worktree in place for recovery.
                            let outcome = super::checkout_txn::rollback_failed(
                                home,
                                &mangled,
                                &mut journal,
                                txn_now,
                                || false,
                                || {},
                            );
                            return rollback_response(
                                outcome,
                                &format!("could not snapshot committed binding: {other:?}"),
                                "binding_snapshot",
                                "bind_full",
                                branch,
                            );
                        }
                    };
                #[cfg(test)]
                crate::worktree_pool::release_test_seam::hit(
                    crate::worktree_pool::ReleaseTestPhase::CheckoutBoundBeforeCommit,
                );
                // #2158 GR1: self-claim checkout does NOT auto-arm ci_watch
                // (dispatch path arms via arm_ci_watch=true; self-claim uses
                // `ci action=watch` explicitly).
                resp["bound"] = json!(true);
                resp["ci_watch_armed"] = json!(false);
                resp["auto_created_branch"] = json!(auto_created_branch);
                resp["fetch_attempted"] = json!(fetch_attempted);
                if checkout_purpose == Some(CheckoutPurpose::DisposableReview) {
                    resp["checkout_purpose"] = json!("disposable_review");
                }
            }
            // #2755 Committed: the durable linearization point. Success is returned
            // ONLY after this journal write lands; a store::atomic_write failure
            // aborts into rollback (worktree + binding), never a half-visible
            // provision.
            journal.advance(super::checkout_txn::Phase::Committed);
            if journal.save(home, &mangled).is_err() {
                let fingerprint = bound_fingerprint.as_ref();
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    || {
                        if let Some(fingerprint) = fingerprint {
                            let released =
                                crate::worktree_pool::release_bound_target_exact_under_branch_lock_with_permit(
                                    home,
                                    instance_name,
                                    fingerprint,
                                    &worktree_dir,
                                    &source_canonical,
                                    lifecycle_permit
                                        .as_ref()
                                        .expect("bind permit held for checkout rollback"),
                                );
                            released.released
                        } else {
                            remove_worktree()
                        }
                    },
                    || {},
                );
                return rollback_response(
                    outcome,
                    "commit journal write failed",
                    "commit_failed",
                    "committed",
                    branch,
                );
            }
            // Committed durable ⇒ transaction resolved; drop the journal tombstone.
            super::checkout_txn::Journal::clear(home, &mangled);
            // #6: echo expected_head/actual_head only when the caller supplied it
            // (omitted → no new fields, byte-compatible with pre-#6 callers).
            // Re-read the actual HEAD from the provisioned worktree rather than
            // echoing the expected value — the worktree is the ground truth.
            if let Some(expected) = args["expected_head"].as_str() {
                let actual = crate::git_helpers::git_cmd(
                    Path::new(&worktree_path_str),
                    &["rev-parse", "HEAD"],
                )
                .unwrap_or_default();
                let actual = actual.trim();
                resp["actual_head"] = json!(actual);
                resp["expected_head"] = json!(expected);
            }
            resp
        }
        Ok(o) => {
            // Prepared journal but `git worktree add` failed ⇒ no worktree to roll
            // back; drop the journal.
            super::checkout_txn::Journal::clear(home, &mangled);
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let redacted = redact_paths(stderr.trim());
            let mut err = json!({
                "error": format!("git worktree add failed: {redacted}"),
                "code": "worktree_add_failed",
                "stage": "worktree_add",
                "raw": redacted,
            });
            if bind {
                err["fetch_attempted"] = json!(fetch_attempted);
                err["auto_created_branch"] = json!(auto_created_branch);
            }
            err
        }
        Err(e) => {
            super::checkout_txn::Journal::clear(home, &mangled);
            let spawn_err = redact_paths(&e.to_string());
            let mut err = json!({
                "error": format!("git worktree add spawn failed: {spawn_err}"),
                "code": "worktree_add_failed",
                "stage": "worktree_add",
                "raw": spawn_err,
            });
            if bind {
                err["fetch_attempted"] = json!(fetch_attempted);
                err["auto_created_branch"] = json!(auto_created_branch);
            }
            err
        }
    }
}
