use crate::agent_ops::validate_branch;
use crate::git_helpers::git_bypass;
use serde_json::{json, Value};
use std::path::Path;

/// #2755 structured redaction: replace absolute filesystem paths (and Windows
/// drive paths) in an error string RETURNED over the wire with `<path>`. The
/// structured `code`/`stage`/`branch` stay actionable for the caller, but raw
/// paths / git stderr — which leak the local layout, usernames, or submodule
/// URLs — are stripped. The FULL, un-redacted detail is still recorded in the
/// daemon event-log (`log_checkout_outcome`), so operators keep debuggability.
pub(super) fn redact_paths(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // A Windows drive path (`C:\…`), OR a POSIX path of ≥2 segments starting
        // at a non-word boundary — the `≥2` + boundary avoid mangling "and/or"
        // and a URL's "//host". Rust's regex has no lookbehind, so the leading
        // boundary char is captured (`b`) and restored in the replacement.
        regex::Regex::new(r"(?P<b>^|[^\w])(?P<p>[A-Za-z]:\\[\w.\\@~%+-]+|(?:/[\w.@~%+-]+){2,})")
            .expect("valid redaction regex")
    });
    re.replace_all(s, "${b}<path>").into_owned()
}

/// #1447: resolve the checkout source repo from `repository_path` — the
/// cross-tool standard name used by bind_self / team update. Returns `None`
/// when absent or empty.
pub(crate) fn checkout_source(args: &Value) -> Option<&str> {
    args.get("repository_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// #2755 R3 (root + independent review): map a post-`git worktree add`
/// [`RollbackOutcome`](super::checkout_txn::RollbackOutcome) to the checkout error
/// response, reporting the ACTUAL cleanup state. `Removed` → the historical
/// "worktree rolled back" text. `RollbackPending` → a STRUCTURED pending state
/// (`code: "rollback_pending"`, `rollback_pending: true`) that NEVER claims the
/// worktree was rolled back — the remove failed (Windows open-handle / transient
/// FS) and the worktree survives for the recovery sweep. `intent_durable=false`
/// (the retained-intent journal save ALSO failed) is surfaced for intervention.
/// The original failure `code`/`stage` are preserved (`failed_code`/`stage`) so
/// machine consumers keep the root cause. Pure — unit-tested cross-platform.
pub(super) fn rollback_response(
    outcome: super::checkout_txn::RollbackOutcome,
    reason: &str,
    code: &str,
    stage: &str,
    branch: &str,
) -> Value {
    use super::checkout_txn::RollbackOutcome;
    match outcome {
        RollbackOutcome::Removed => json!({
            "error": format!("{reason}, worktree rolled back"),
            "code": code,
            "stage": stage,
            "branch": branch,
        }),
        RollbackOutcome::RollbackPending { intent_durable } => json!({
            "error": format!(
                "{reason}; worktree REMOVE FAILED — rollback pending, recovery sweep will retry{}",
                if intent_durable {
                    ""
                } else {
                    " (retained-intent journal save ALSO failed — operator intervention needed)"
                }
            ),
            "code": "rollback_pending",
            "rollback_pending": true,
            "intent_durable": intent_durable,
            "failed_code": code,
            "stage": stage,
            "branch": branch,
        }),
    }
}

/// #2755 R3 (independent P1.4): fsync the `.agend-managed` marker file's CONTENTS
/// durable — `std::fs::write` + a parent-dir fsync makes the DIRENT durable but not
/// the bytes, so a crash/power loss could leave a durable journal phase (or Committed
/// success) with an empty/torn marker. Open + `sync_all()` and OBSERVE the result; a
/// failure aborts the transaction fail-closed. A `cfg(test)` thread-local seam forces
/// the sync error so the crash/durability rollback path is testable cross-platform.
pub(super) fn sync_marker_contents(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_MARKER_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other(
            "test seam: forced marker sync_all failure",
        ));
    }
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
thread_local! {
    static FAIL_MARKER_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: arm/disarm the [`sync_marker_contents`] failure seam (current thread).
#[cfg(test)]
pub(super) fn set_fail_marker_sync(fail: bool) {
    FAIL_MARKER_SYNC.with(|c| c.set(fail));
}

pub(crate) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let result = handle_checkout_repo_inner(home, args, instance_name);
    log_checkout_outcome(home, args, instance_name, &result);
    result
}

/// #1466: record every `repo action=checkout` outcome — success AND every
/// error path — to the daemon-observable event-log, so a silently-failed
/// checkout (e.g. the partial-worktree bootstrap race that motivated #1466:
/// `src/` present but no `.git`) leaves a diagnosable trace. Reuses
/// `event_log::log` (the same freeform-msg helper as `worktree_released_full`
/// — no new schema). Best-effort: `event_log::log` is fire-and-forget, so a
/// logging failure can never affect the checkout result (observability must
/// not become an availability risk). Logging once at the single wrapper exit
/// guarantees coverage of all current and future return paths.
fn log_checkout_outcome(home: &Path, args: &Value, instance_name: &str, result: &Value) {
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    let source = checkout_source(args).unwrap_or("");
    let ok = result.get("error").is_none();
    let mut msg = format!("branch={branch} source={source} ok={ok}");
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        msg.push_str(&format!(" err={err}"));
    }
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        msg.push_str(&format!(" path={path}"));
    }
    crate::event_log::log(home, "worktree_checkout", instance_name, &msg);
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
    // #778 Option 1: optional atomic provision + bind. When `bind:true`,
    // tail-ops mirror `bind_self → dispatch_auto_bind_lease` (marker +
    // binding.json + ci_watches arm) directly on the just-provisioned
    // worktree. Default `false` preserves existing back-compat callers
    // (review pool, operator triage) that materialize a detached-HEAD
    // inspection worktree without claiming it.
    let bind = args["bind"].as_bool().unwrap_or(false);
    if bind {
        if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
            return e;
        }
    }
    if bind && instance_name.is_empty() {
        return json!({
            "error": "bind=true requires AGEND_INSTANCE_NAME — anonymous callers cannot claim a worktree",
            "code": "needs_identity"
        });
    }
    // Windows-safe path mangling: also collapse `\` (path separator) and
    // `:` (drive letter) so a source like `C:\Users\runner\...` doesn't
    // produce a worktree path with mid-name colons (rejected by NTFS).
    // Pre-existing tests didn't exercise Windows-built happy-path until
    // #778's new bind:true coverage.
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace(['/', '\\', ':'], "_").replace('~', "")
    ));
    std::fs::create_dir_all(worktree_dir.parent().unwrap_or(home)).ok();
    // #2158 PR1: resolve + validate the source repo path fail-closed (absolute or
    // known agent name only; canonicalize; reject system dirs). Extracted to
    // `source_resolve` — keeps this oversized handler under the file_size ceiling
    // (t-61 split debt) and isolates the security-sensitive resolution for review.
    let (source_path, source_canonical) =
        match super::source_resolve::resolve_checkout_source_path(home, source) {
            Ok(pair) => pair,
            Err(e) => return e,
        };
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
        match crate::mcp::handlers::dispatch_hook::ensure_branch_exists(
            home,
            src,
            branch,
            from_ref,
            instance_name,
        ) {
            Ok((created, fetched)) => {
                auto_created_branch = created;
                fetch_attempted = fetched;
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
                    let wt_str = wt.display().to_string();
                    tracing::info!(
                        instance = instance_name,
                        %branch,
                        path = %wt_str,
                        "repo checkout bind:true idempotent — agent already bound to this branch, revalidating + self-healing existing worktree"
                    );
                    // #2755 R3 (B4): the binding's worktree `wt` may be a DIFFERENT path
                    // than the DERIVED `worktree_dir` the path-lock A guards (normal
                    // dispatch layout worktrees/<agent>/<branch> vs the derived
                    // <agent>-<source>). Mutating `wt` under A is a lock-for-the-wrong-
                    // path hole. Under the OUTER branch-lease (held), DROP A and acquire
                    // the EXACT lock B for `wt` (no A→B inversion → no deadlock), then CAS
                    // re-read the binding + validate provenance BEFORE any destructive
                    // sync/reset/init.
                    drop(path_lock); // release A; the branch-lease still serializes us
                    let wt_mangled = wt
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();
                    let wt_lock = match super::checkout_txn::acquire_path_lock(
                        home,
                        &wt,
                        &wt_mangled,
                    ) {
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
                    // CAS re-read + provenance from ONE fresh read under lock B. The
                    // binding must STILL map this exact branch+worktree (a concurrent
                    // release/rebind may have changed it), AND — fail closed (decision
                    // d-…38; signature verification is out of #2755 scope) — the bound
                    // worktree must be a DAEMON-MANAGED worktree (`.agend-managed` marker,
                    // within the daemon worktree area) of the REQUESTED source.
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
                        .map(|c| c == source_canonical)
                        .unwrap_or(false);
                    let managed = wt.join(crate::worktree_pool::MANAGED_MARKER).is_file()
                        && wt.starts_with(home.join("worktrees"));
                    if !bound_source_ok || !managed {
                        return json!({
                            "error": "reuse refused: the bound worktree is not a daemon-managed worktree of the requested source at the exact bound path",
                            "code": "reuse_provenance",
                            "branch": branch,
                        });
                    }
                    // #2755 R3 (B1): sync to the FINAL HEAD FIRST (an externally advanced
                    // branch may change/add gitlinks), THEN strict recursive init, THEN
                    // verify EXACT gitlink commits — any sync/init/verify failure returns
                    // NO success (fail closed), never a bound:true over a broken tree.
                    if let Err(e) = crate::worktree::sync_worktree_to_head_strict(&wt) {
                        return json!({
                            "error": format!("reuse: sync to HEAD failed: {}", redact_paths(&e)),
                            "code": "reuse_sync_failed",
                            "branch": branch,
                        });
                    }
                    if let Err(e) = crate::worktree::init_submodules_strict(&wt) {
                        return json!({
                            "error": format!(
                                "reuse: recursive submodule init failed: {}",
                                redact_paths(&e)
                            ),
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
                    return json!({
                        "path": wt_str,
                        "source": source_path,
                        "branch": branch,
                        "bound": true,
                        "idempotent": true,
                        "auto_created_branch": auto_created_branch,
                        "fetch_attempted": fetch_attempted,
                    });
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
            let marker_path = worktree_dir.join(crate::worktree_pool::MANAGED_MARKER);
            if std::fs::write(
                &marker_path,
                format!(
                    "agent={instance_name}\nbranch={branch}\nleased_at={}\n",
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
            crate::store::fsync_parent_dir(&marker_path); // dirent durability
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
            if bind {
                // #2533: optional caller-supplied task_id — attributes this self-claim
                // to a task (§3.19.1 reviewer checkout is the common case) so `bind_full`
                // treats it as in-dispatch instead of warning. Absent → "" (unattributed,
                // pre-#2533 behavior unchanged).
                let task_id = args["task_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("");
                if let Err(e) = crate::binding::bind_full(
                    home,
                    instance_name,
                    task_id,
                    branch,
                    &worktree_dir,
                    &source_canonical,
                    true, // #2158 GR1: agent self-claim (repo checkout bind:true) → notify operator
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
                    return rollback_response(
                        outcome,
                        &format!("bind_full failed: {}", redact_paths(&e.to_string())),
                        "bind_rollback",
                        "bind_full",
                        branch,
                    );
                }
                // #2158 GR1 (operator-approved): a self-claimed `repo action=checkout
                // bind=true` no longer SILENTLY arms a ci_watch — neither here (this
                // inline arm is removed) NOR via the shared dispatch_hook path
                // (`bind_self` self-claims pass `arm_ci_watch=false`). The silent
                // auto-arm was part of the #2158 incident blast: a transient sub-agent
                // (sharing the primary's identity) self-claiming a worktree also armed a
                // watch the operator never asked for. The daemon DISPATCH path passes
                // `arm_ci_watch=true` and STILL arms for normal delegation. A
                // self-claiming agent that wants CI notifications arms it explicitly via
                // `ci action=watch`.
                resp["bound"] = json!(true);
                resp["ci_watch_armed"] = json!(false);
                resp["auto_created_branch"] = json!(auto_created_branch);
                resp["fetch_attempted"] = json!(fetch_attempted);
            }
            // #2755 Committed: the durable linearization point. Success is returned
            // ONLY after this journal write lands; a store::atomic_write failure
            // aborts into rollback (worktree + binding), never a half-visible
            // provision.
            journal.advance(super::checkout_txn::Phase::Committed);
            if journal.save(home, &mangled).is_err() {
                let outcome = super::checkout_txn::rollback_failed(
                    home,
                    &mangled,
                    &mut journal,
                    txn_now,
                    remove_worktree,
                    || {
                        if bind {
                            crate::binding::unbind(home, instance_name);
                        }
                    },
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
