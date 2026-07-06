use serde_json::{json, Value};

/// #t-…83936-6 P0 (data-loss incident): is `p` a git PRIMARY/main working tree
/// (i.e. a real source repo), as opposed to a linked worktree?
///
/// A main working tree's `.git` is a DIRECTORY (the actual object store); a
/// linked worktree's `.git` is a gitlink FILE (`gitdir: <repo>/.git/worktrees/…`).
/// `handle_release_repo`'s `remove_dir_all` fallback runs whenever
/// `git worktree remove` fails — and git ALWAYS refuses to remove a main working
/// tree — so without this guard a `repo release` pointed at a canonical repo
/// deletes the ENTIRE repo (the 2026-07-06 canonical-deletion incident).
pub(crate) fn is_primary_working_tree(p: &std::path::Path) -> bool {
    p.join(".git").is_dir()
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
    // #t-…83936-6 P0: NEVER release a primary/main working tree — the fallback
    // below would `remove_dir_all` the whole source repo (canonical-deletion
    // incident). A releasable worktree is a LINKED one (`.git` is a gitlink file).
    if is_primary_working_tree(&canonical) {
        return Err(format!(
            "rejected: primary working tree (source repo), not a releasable worktree: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

pub(crate) fn handle_release_repo(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return json!({"error": "missing 'path'"}),
    };

    // H3 fix: validate + canonicalize path before any filesystem ops.
    let canonical = match validate_release_path(path) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
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
            // #t-…83936-6 depth guard: NEVER `remove_dir_all` a primary working
            // tree even if `git worktree remove` failed and validate was somehow
            // bypassed — this fallback is what deleted the canonical.
            if !is_primary_working_tree(&canonical) {
                let _ = std::fs::remove_dir_all(&canonical);
            }
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            if !is_primary_working_tree(&canonical) {
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
