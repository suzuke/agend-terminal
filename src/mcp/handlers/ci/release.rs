use serde_json::{json, Value};
use std::path::Path;

/// arch14 (d-20260720044124067125-6): `source_repo` is `None` when the LINE is
/// missing (a pre-#2860 legacy marker — the absent-binding arm's only legal
/// input) vs `Some("")` for an explicit blank value (always refused).
fn parse_managed_marker(wt: &Path) -> Option<(String, String, Option<String>)> {
    let content = std::fs::read_to_string(wt.join(crate::worktree_pool::MANAGED_MARKER)).ok()?;
    let get = |prefix: &str| {
        content
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|s| s.trim().to_string())
    };
    let agent = get("agent=").unwrap_or_default();
    if agent.is_empty() {
        return None;
    }
    Some((
        agent,
        get("branch=").unwrap_or_default(),
        get("source_repo="),
    ))
}

/// Derive the source repo from a linked worktree's `.git` gitlink file
/// (`gitdir: <src>/.git/worktrees/<name>`). Shared by the non-managed removal
/// path (post-removal prune) and the arch14 absent-binding arm (branch-lease
/// lock key). `None` on any unexpected shape — callers stay fail-safe.
fn derive_source_from_gitlink(canonical: &Path) -> Option<std::path::PathBuf> {
    canonical
        .join(".git")
        .is_file()
        .then(|| std::fs::read_to_string(canonical.join(".git")).ok())
        .flatten()
        .and_then(|content| {
            let gitdir = content.strip_prefix("gitdir: ")?.trim();
            let p = std::path::Path::new(gitdir);
            p.parent()?.parent()?.parent().map(|pp| pp.to_path_buf())
        })
}

fn delegate_managed_release(
    home: &Path,
    canonical: &Path,
    caller: &str,
    nested_discard: Option<&crate::worktree_pool::NestedDirtDiscard<'_>>,
) -> Value {
    let Some((marker_agent, _, _)) = parse_managed_marker(canonical) else {
        return json!({
            "error": "managed worktree marker unreadable or missing agent — refusing (fail-closed)",
            "code": "managed_marker_invalid",
        });
    };
    let fingerprint = match crate::binding::snapshot_guarded_binding(home, &marker_agent) {
        Ok(crate::binding::GuardedBinding::Known { value, fingerprint }) => {
            let bound_wt = value["worktree"].as_str().unwrap_or("");
            if bound_wt != canonical.to_str().unwrap_or("") {
                return json!({
                    "error": format!(
                        "managed marker claims agent '{}' but binding worktree '{}' \
                         does not match requested path '{}' — refusing (stale marker)",
                        marker_agent, bound_wt, canonical.display()
                    ),
                    "code": "managed_release_path_mismatch",
                });
            }
            fingerprint
        }
        Ok(crate::binding::GuardedBinding::Absent) => {
            // arch14 (d-20260720044124067125-6): a legacy sourceless-but-
            // otherwise-valid marker whose binding no longer exists may release
            // via the target's OWN verified .git linkage — every other absent-
            // binding shape keeps the fail-closed refusal below.
            return absent_binding_legacy_release(
                home,
                canonical,
                caller,
                &marker_agent,
                nested_discard,
            );
        }
        Ok(crate::binding::GuardedBinding::Opaque(reason)) | Err(reason) => {
            return json!({
                "error": format!("binding opaque for '{}': {reason} — refusing", marker_agent),
                "code": "managed_release_opaque",
            });
        }
    };
    if !caller.is_empty()
        && caller != marker_agent
        && !crate::teams::is_orchestrator_of(home, caller, &marker_agent)
    {
        return json!({
            "error": format!(
                "caller '{}' not authorized to release agent '{}' worktree",
                caller, marker_agent
            ),
            "code": "managed_release_unauthorized",
        });
    }
    if nested_discard.is_some() {
        return json!({
            "error": "nested dirt discard is not supported for Known-bound managed releases; \
                      resolve nested dirt in place before releasing",
            "code": "discard_unsupported_release_path",
        });
    }
    let outcome = crate::worktree_pool::release_full_exact(home, &marker_agent, &fingerprint, true);
    json!({
        "path": canonical.display().to_string(),
        "delegated_to_canonical": true,
        "released": outcome.released,
        "worktree_removed": outcome.worktree_removed,
        "binding_removed": outcome.binding_removed,
        "branch_deleted": outcome.branch_deleted,
        "error": outcome.error,
    })
}

/// arch14 absent-binding arm (d-20260720044124067125-6): a LEGACY marker —
/// non-empty agent AND branch, source_repo LINE MISSING (pre-#2860 producer
/// format) — whose binding no longer exists may release via the target's own
/// verified Git linkage. Everything else keeps the fail-closed
/// `managed_release_no_binding` refusal. Caller authority is the SAME set as
/// the bound path (marker agent / its orchestrator / anonymous operator).
/// Identity is path-anchored: the gitlink names the source, the checked-out
/// branch must equal the marker's branch, and under the existing
/// branch→agent→binding lock order the binding must STILL be absent (a bind
/// that raced in wins). The removal itself reuses the canonical
/// linked-worktree removal path while the locks are held — the agent lock
/// blocks a concurrent re-provision of this exact `worktrees/<agent>/<branch>`
/// path for the removal's duration.
fn absent_binding_legacy_release(
    home: &Path,
    canonical: &Path,
    caller: &str,
    marker_agent: &str,
    nested_discard: Option<&crate::worktree_pool::NestedDirtDiscard<'_>>,
) -> Value {
    let refuse_no_binding = || {
        json!({
            "error": format!(
                "managed marker claims agent '{marker_agent}' but no binding exists — refusing"
            ),
            "code": "managed_release_no_binding",
        })
    };
    let Some((_, mk_branch, mk_source)) = parse_managed_marker(canonical) else {
        return refuse_no_binding();
    };
    // Only the missing-LINE legacy shape qualifies; an explicit (even blank)
    // source_repo value or a branchless marker stays refused.
    if mk_source.is_some() || mk_branch.is_empty() {
        return refuse_no_binding();
    }
    if !caller.is_empty()
        && caller != marker_agent
        && !crate::teams::is_orchestrator_of(home, caller, marker_agent)
    {
        return json!({
            "error": format!(
                "caller '{caller}' not authorized to release agent '{marker_agent}' worktree"
            ),
            "code": "managed_release_unauthorized",
        });
    }
    // Canonical Git identity (GREEN-2, d-20260720053617389698-7): git's OWN
    // common-dir resolution is the source authority — never lexical gitlink
    // text arithmetic, so a VALID relative gitlink works exactly like the
    // absolute form. `--path-format=absolute` makes the answer directly
    // canonicalizable regardless of the gitlink's spelling.
    let common_dir = match crate::git_helpers::git_cmd(
        canonical,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        Ok(out) => std::path::PathBuf::from(out.trim()),
        Err(e) => {
            return json!({
                "error": format!(
                    "legacy marker for '{marker_agent}': worktree Git linkage unverifiable: {e} — refusing"
                ),
                "code": "managed_release_unverified_linkage",
            });
        }
    };
    let Some(source_parent) = common_dir.parent() else {
        return json!({
            "error": format!(
                "legacy marker for '{marker_agent}': common-dir '{}' has no parent source — refusing",
                common_dir.display()
            ),
            "code": "managed_release_unverified_linkage",
        });
    };
    let Ok(source_canonical) = std::fs::canonicalize(source_parent) else {
        return json!({
            "error": format!(
                "legacy marker for '{marker_agent}': linked source '{}' cannot be canonicalized — refusing",
                source_parent.display()
            ),
            "code": "managed_release_unverified_linkage",
        });
    };
    // The checked-out branch must equal the marker's branch identity (pre-gate;
    // the canonical transaction re-validates marker identity post-seam).
    let head_branch = match crate::git_helpers::git_cmd(
        canonical,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    ) {
        Ok(out) => out.trim().to_string(),
        Err(e) => {
            return json!({
                "error": format!(
                    "legacy marker for '{marker_agent}': cannot resolve worktree HEAD branch: {e} — refusing"
                ),
                "code": "managed_release_unverified_linkage",
            });
        }
    };
    if head_branch != mk_branch {
        return json!({
            "error": format!(
                "legacy marker branch '{mk_branch}' does not match checked-out branch '{head_branch}' — refusing"
            ),
            "code": "managed_release_branch_mismatch",
        });
    }
    // GREEN-2: converge on the CANONICAL absent-target transaction. The caller
    // holds the lifecycle permit and L(repo,branch); the transaction acquires
    // A→B, re-reads the binding as truly absent AFTER the
    // ReleaseTestPhase::AfterBindingSnapshot seam, re-validates the marker's
    // agent/branch and the target's common-dir identity
    // (target_source_repo_matches), preserves dirty WIP, and removes via the
    // exact-metadata-aware path — no second removal implementation.
    let permit = match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        marker_agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    ) {
        Ok(permit) => permit,
        Err(e) => {
            return json!({
                "error": format!("release refused: bind/rebase in flight; {e}"),
                "code": "managed_release_lock_failed",
            });
        }
    };
    let source_repo_str = source_canonical.display().to_string();
    let _branch_lock =
        match crate::binding::acquire_branch_lease_lock(home, &source_repo_str, &mk_branch) {
            Ok(lock) => lock,
            Err(e) => {
                return json!({
                    "error": format!("branch lease lock failed: {e} — refusing"),
                    "code": "managed_release_lock_failed",
                });
            }
        };
    let sender = (!caller.is_empty()).then_some(caller);
    let outcome = crate::worktree_pool::release_absent_target_under_branch_lock(
        home,
        marker_agent,
        &mk_branch,
        canonical,
        &source_canonical,
        sender,
        &permit,
        nested_discard,
    );
    let mut resp = json!({
        "path": canonical.display().to_string(),
        "legacy_absent_binding": true,
        "released": outcome.released,
        "worktree_removed": outcome.worktree_removed,
        "git_metadata_pruned": outcome.git_metadata_pruned,
        "error": outcome.error,
    });
    if let Some(ref digest) = outcome.nested_dirt_digest {
        resp["nested_dirt_digest"] = json!(digest);
    }
    resp
}

/// #t-…83936-6 P0 (data-loss incident): is `p` a TRUE linked git worktree — the
/// ONLY shape `handle_release_repo`'s `remove_dir_all` fallback may delete?
///
/// GIT is the source of truth, NOT a filesystem heuristic. A `.git`-is-a-file
/// test can't tell a linked worktree from a `--separate-git-dir` MAIN tree —
/// both have a gitlink file (reviewer4); a `.git`-is-a-dir blacklist missed BARE
/// repos (reviewer5). A linked worktree is the UNIQUE shape whose per-worktree
/// git-dir (`.../worktrees/<name>`) differs from the shared common-dir (the main
/// `.git`) AND that is non-bare. For main / bare / separate-git-dir-main, git-dir
/// == common-dir. Any git failure or unexpected output ⇒ `false` (FAIL-SAFE:
/// prefer a false reject over deleting a repo; a stale/orphan worktree whose
/// admin entry was pruned is left for other cleanup, never `remove_dir_all`'d
/// here). Without this, a `repo release` on a canonical / bare / separate-git-dir
/// repo deletes the ENTIRE repo (the 2026-07-06 canonical-deletion incident).
fn is_linked_worktree(p: &std::path::Path) -> bool {
    // Route through the sanctioned `git_helpers::git_cmd` (always-bypass, bounded)
    // — a raw git subprocess here would trip the daemon-git invariant
    // (tests/daemon_git_helper_invariant.rs). `git_cmd` runs from `p` as its cwd,
    // so no `-C` is needed; `--path-format=absolute` makes git-dir/common-dir
    // directly comparable.
    let Ok(out) = crate::git_helpers::git_cmd(
        p,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--is-bare-repository",
        ],
    ) else {
        return false; // not a repo / git failed → unclassifiable → refuse (fail-safe)
    };
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    match lines.as_slice() {
        [git_dir, common_dir, is_bare] => git_dir != common_dir && *is_bare == "false",
        _ => false, // unexpected output shape → refuse
    }
}

/// Reject paths that would be dangerous to `remove_dir_all`.
/// Validate and canonicalize a release path. Returns canonical absolute
/// path on success, or error message on rejection.
pub(crate) fn validate_release_path(path_str: &str) -> Result<std::path::PathBuf, String> {
    let path_str = path_str.trim();
    if path_str.is_empty() {
        return Err("rejected: empty path".into());
    }
    let path = std::path::Path::new(path_str);
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("path does not exist or unreadable: {e}"))?;
    if canonical.parent().is_none() {
        return Err(format!("rejected: root: {}", canonical.display()));
    }
    if let Ok(home) = std::env::var("HOME") {
        if canonical == std::path::Path::new(&home) {
            return Err(format!("rejected: HOME: {}", canonical.display()));
        }
    }
    let system_prefixes: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows",
            "C:\\Program Files",
            "C:\\Program Files (x86)",
            "C:\\ProgramData",
        ]
    } else {
        &[
            "/etc",
            "/usr",
            "/var",
            "/bin",
            "/sbin",
            "/boot",
            "/sys",
            "/proc",
            "/dev",
            "/Library",
            "/System",
            "/Applications",
            "/opt",
            "/tmp",
            "/private",
        ]
    };
    for prefix in system_prefixes {
        if canonical.starts_with(prefix) {
            return Err(format!("rejected: system path: {}", canonical.display()));
        }
    }
    if canonical.components().count() < 3 {
        return Err(format!("rejected: too shallow: {}", canonical.display()));
    }
    // #t-…83936-6 P0: WHITELIST — the only safely-releasable shape is a LINKED
    // worktree (`.git` is a gitlink file). Refuse main (`.git` is a dir), bare (no
    // `.git` child — it IS a git dir), and non-repo dirs, else the `remove_dir_all`
    // fallback nukes the whole source repo (canonical-deletion incident; reviewer5
    // bare-repo bypass of the earlier blacklist).
    if !is_linked_worktree(&canonical) {
        return Err(format!(
            "rejected: not a releasable linked worktree (main/bare/non-repo): {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

pub(crate) fn handle_release_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return json!({"error": "missing 'path'"}),
    };

    let discard_nested = args["discard_nested_dirt"].as_bool().unwrap_or(false);
    let force = args["force"].as_bool().unwrap_or(false);
    let expected_digest = args["expected_nested_dirt_digest"].as_str();
    let audit_reason = args["audit_reason"].as_str().unwrap_or("");

    let nested_discard = if discard_nested {
        if !force {
            return json!({"error": "nested dirt discard requires force=true"});
        }
        match expected_digest {
            Some(d) if !d.trim().is_empty() => {
                let reason = audit_reason.trim();
                if reason.is_empty() {
                    return json!({"error": "nested dirt discard requires non-empty audit_reason"});
                }
                Some(crate::worktree_pool::NestedDirtDiscard {
                    expected_digest: d,
                    audit_reason: reason,
                })
            }
            _ => {
                return json!({"error": "nested dirt discard requires expected_nested_dirt_digest"});
            }
        }
    } else {
        None
    };

    let canonical = match validate_release_path(path) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };

    if crate::worktree_pool::is_daemon_managed(&canonical) {
        return delegate_managed_release(home, &canonical, instance_name, nested_discard.as_ref());
    }

    if nested_discard.is_some() {
        return json!({
            "error": "nested dirt discard is not supported for unmanaged worktrees; \
                      resolve nested dirt in place before releasing",
            "code": "discard_unsupported_release_path",
        });
    }

    match crate::binding::worktree_binding_state(home, &canonical) {
        crate::binding::WorktreeBindingState::Bound => {
            return json!({
                "error": "release refused: marker absent but an authoritative binding \
                          targets this worktree — restore the marker or use the managed \
                          release path",
                "code": "markerless_bound_worktree",
            });
        }
        crate::binding::WorktreeBindingState::Uncertain => {
            return json!({
                "error": "release refused: marker absent and binding state is uncertain \
                          (unreadable or unparseable binding present) — cannot safely \
                          determine ownership; resolve the binding or restore the marker",
                "code": "markerless_uncertain_binding",
            });
        }
        crate::binding::WorktreeBindingState::Unbound => {}
    }

    remove_linked_worktree_canonical(&canonical, path)
}

/// The canonical linked-worktree removal: bounded `git worktree remove
/// --force`, the #t-…83936-6 whitelist-guarded `remove_dir_all` fallback, and
/// the post-removal source prune. Extracted verbatim from
/// `handle_release_repo`'s tail so the arch14 absent-binding arm reuses the
/// EXACT same path (no second removal implementation).
fn remove_linked_worktree_canonical(canonical: &Path, path: &str) -> Value {
    let path_str = canonical.to_string_lossy();

    // Derive source repo from worktree .git link before any removal —
    // needed for post-removal prune if git worktree remove fails.
    let source_repo = derive_source_from_gitlink(canonical);

    // #1899: bounded via spawn_group_bounded with a BARE Command — this site
    // deliberately does NOT set AGEND_GIT_BYPASS and does NOT set current_dir
    // (runs from the daemon cwd, best-effort). Preserve that exact behaviour;
    // spawn_group_bounded only adds the LOCAL timeout + safe process-group kill,
    // without forcing the bypass env. (Whether it SHOULD bypass like ci/mod:270
    // is a separate behaviour question, out of scope for this timeout PR.)
    // git-raw-allowed: deliberate non-bypass + no current_dir; already bounded via
    // spawn_group_bounded; the Ok(non-zero) arm surfaces stderr in the JSON `note`
    // (git_ok would discard it), so git_cmd/git_ok would not be byte-identical.
    //
    // #2550 W2 (git_worktree.rs primitives module): this is the ONE
    // `worktree remove --force` call site NOT converged onto
    // `git_worktree::remove_force` — that helper always either bypasses
    // (non-empty repo) or sets AGEND_GIT_BYPASS without a cwd (empty repo),
    // neither of which matches this site's deliberate no-bypass/no-cwd
    // shape. Lead's decision (`m-20260703064336281447-62`): a mechanical
    // consolidation PR must not silently resolve this still-open "should it
    // bypass like ci/mod:270" question above — left as-is, tracked as an
    // open item for operator/a future lead, not part of W2.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["worktree", "remove", "--force", &path_str]);
    let result = match crate::git_helpers::spawn_group_bounded(
        cmd,
        "git worktree remove (cleanup)",
        crate::git_helpers::LOCAL_GIT_TIMEOUT,
    ) {
        Ok(o) if o.status.success() => return json!({"path": path}),
        Ok(o) => {
            // #t-…83936-6 depth guard (WHITELIST): only `remove_dir_all` a LINKED
            // worktree, even if `git worktree remove` failed and validate was
            // somehow bypassed — this fallback is what deleted the canonical/bare.
            if is_linked_worktree(canonical) {
                let _ = std::fs::remove_dir_all(canonical);
            }
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            if is_linked_worktree(canonical) {
                let _ = std::fs::remove_dir_all(canonical);
            }
            json!({"path": path})
        }
    };
    // CR-2026-06-14: a fallback arm force-removed the working tree — prune the
    // source's stale `.git/worktrees` metadata, or warn it'll leak if unresolved.
    if let Some(src) = &source_repo {
        crate::worktree::prune(src);
    } else {
        tracing::warn!(path = %path_str, "release_repo: source repo unresolved — stale `.git/worktrees` metadata may leak; run force_release / GC");
    }
    result
}
