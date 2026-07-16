use serde_json::{json, Value};
use std::path::Path;

fn parse_managed_marker(wt: &Path) -> Option<(String, String, String)> {
    let content = std::fs::read_to_string(wt.join(crate::worktree_pool::MANAGED_MARKER)).ok()?;
    let get = |prefix: &str| {
        content
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let agent = get("agent=");
    if agent.is_empty() {
        return None;
    }
    Some((agent, get("branch="), get("source_repo=")))
}

fn delegate_managed_release(home: &Path, canonical: &Path, caller: &str) -> Value {
    let Some((marker_agent, marker_branch, marker_repo)) = parse_managed_marker(canonical) else {
        return json!({
            "error": "managed worktree marker unreadable or missing agent — refusing (fail-closed)",
            "code": "managed_marker_invalid",
        });
    };
    let fingerprint = match crate::binding::snapshot_guarded_binding(home, &marker_agent) {
        Ok(crate::binding::GuardedBinding::Known { value, fingerprint }) => {
            let bound_wt = value["worktree"].as_str().unwrap_or("");
            let bound_branch = value["branch"].as_str().unwrap_or("");
            let bound_source = value["source_repo"].as_str().unwrap_or("");
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
            if !marker_branch.is_empty() && bound_branch != marker_branch {
                return json!({
                    "error": format!(
                        "marker branch '{}' does not match binding branch '{}' — refusing",
                        marker_branch, bound_branch
                    ),
                    "code": "managed_release_branch_mismatch",
                });
            }
            if !marker_repo.is_empty() && !bound_source.is_empty() && bound_source != marker_repo {
                return json!({
                    "error": format!(
                        "marker source_repo '{}' does not match binding source_repo '{}' — refusing",
                        marker_repo, bound_source
                    ),
                    "code": "managed_release_source_mismatch",
                });
            }
            fingerprint
        }
        Ok(crate::binding::GuardedBinding::Absent) => {
            return json!({
                "error": format!(
                    "managed marker claims agent '{}' but no binding exists — refusing",
                    marker_agent
                ),
                "code": "managed_release_no_binding",
            });
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
    let outcome = crate::worktree_pool::release_full_exact(home, &marker_agent, &fingerprint);
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

    let canonical = match validate_release_path(path) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };

    if crate::worktree_pool::is_daemon_managed(&canonical) {
        return delegate_managed_release(home, &canonical, instance_name);
    }

    let path_str = canonical.to_string_lossy();

    // Derive source repo from worktree .git link before any removal —
    // needed for post-removal prune if git worktree remove fails.
    let source_repo = canonical
        .join(".git")
        .is_file()
        .then(|| std::fs::read_to_string(canonical.join(".git")).ok())
        .flatten()
        .and_then(|content| {
            let gitdir = content.strip_prefix("gitdir: ")?.trim();
            let p = std::path::Path::new(gitdir);
            p.parent()?.parent()?.parent().map(|pp| pp.to_path_buf())
        });

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
            if is_linked_worktree(&canonical) {
                let _ = std::fs::remove_dir_all(&canonical);
            }
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            if is_linked_worktree(&canonical) {
                let _ = std::fs::remove_dir_all(&canonical);
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
